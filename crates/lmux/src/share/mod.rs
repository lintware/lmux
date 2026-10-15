//! `lmux share`: serve the session list and live terminals to a browser, and
//! reach the laptop through a Cloudflare Quick Tunnel.
//!
//! A terminal is remote code execution, so the rules are strict: the link
//! carries a single-use code in the URL fragment (never sent to a server),
//! the page trades it for an `HttpOnly` cookie that lapses when unused, the WebSocket
//! checks `Origin` and that cookie, and typing is off unless asked for.
//! Screen contents are never logged.

mod agents;
mod host;
mod remote;
mod server;
mod service;
mod tunnel;
mod upload;
mod voice;
mod voice_tools;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde_json::json;

/// How long an unused code works.
const CODE_TTL: Duration = Duration::from_secs(300);
/// How long a session lasts without a connected page or app. Every check of a
/// live socket pushes this out, so a phone that keeps using it stays in.
const SESSION_IDLE: Duration = Duration::from_secs(24 * 3600);
/// The longest a session (and its cookie) lasts, used or not.
const SESSION_MAX: Duration = Duration::from_secs(30 * 24 * 3600);

pub struct Opts {
    pub socket: String,
    pub allow_typing: bool,
    pub port: u16,
    pub no_tunnel: bool,
    pub cloudflared: Option<String>,
    pub json: bool,
    /// SSH targets of other servers to control too (gateway mode).
    pub servers: Vec<String>,
    /// Run as a long-lived gateway service: no first code, a control socket
    /// that hands out codes (`lmux share-code`), and exit when the tunnel dies
    /// so the service manager starts a fresh one.
    pub service: bool,
}

pub use remote::backend;
pub use remote::connect_gateway;
pub use service::attach;
pub use service::request_code;

pub async fn run(opts: Opts) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port))
        .await
        .context("binding the local share port")?;
    let port = listener.local_addr()?.port();
    let shared = Arc::new(server::Shared::with_servers(opts.socket.clone(), opts.servers.clone(), opts.allow_typing));
    tokio::spawn(server::serve(shared.clone(), listener));

    // Keep the tunnel handle alive: dropping it kills cloudflared.
    let (base, mut tunnel) = if opts.no_tunnel {
        (format!("http://127.0.0.1:{port}"), None)
    } else {
        if !opts.json {
            eprintln!("Starting a Cloudflare Quick Tunnel…");
        }
        let t = tunnel::start(port, opts.cloudflared.as_deref(), !opts.json, opts.service).await?;
        (t.url.clone(), Some(t))
    };

    if opts.service {
        return service::run(shared, base, tunnel).await;
    }

    let code = shared.new_code();
    let url = format!("{base}/#{code}");
    let qr = qrcode::QrCode::new(url.as_bytes()).context("encoding the QR code")?;

    if opts.json {
        println!("{}", code_json(&url, &shared)?);
        // The app owns this process: when it closes our stdin or exits,
        // sharing ends.
        let mut sink = [0u8; 64];
        use tokio::io::AsyncReadExt;
        let mut stdin = tokio::io::stdin();
        let closed = async { while matches!(stdin.read(&mut sink).await, Ok(n) if n > 0) {} };
        tokio::select! { _ = closed => {}, _ = stop_signal() => {}, _ = tunnel_exit(&mut tunnel) => {} }
        // Files sent from the phone live only as long as the share.
        let _ = std::fs::remove_dir_all(shared.uploads_dir());
        return Ok(());
    }

    use qrcode::render::unicode::Dense1x2;
    println!("{}", qr.render::<Dense1x2>().quiet_zone(true).build());
    println!("{url}\n");
    println!(
        "Mode: {}.",
        if opts.allow_typing { "typing allowed" } else { "view only (add --allow-typing to type)" }
    );
    if let Some(label) = shared.voice_label() {
        println!("Voice: on ({label}). Tap Talk on the page.");
    }
    println!("Scan the code or open the link on your phone within {} minutes; it works once.", CODE_TTL.as_secs() / 60);
    if !opts.no_tunnel {
        println!(
            "Quick Tunnel limits: the address changes every start, there is no uptime guarantee, \
             and Cloudflare answers 429 above 200 requests in flight."
        );
    }
    println!("Press Ctrl-C to stop sharing.");
    tokio::select! { _ = stop_signal() => {}, _ = tunnel_exit(&mut tunnel) => eprintln!("The tunnel stopped.") }
    let _ = std::fs::remove_dir_all(shared.uploads_dir());
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM, so the tunnel is dropped (and cloudflared
/// killed) instead of being orphaned.
async fn stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} },
        Err(_) => {
            tokio::signal::ctrl_c().await.ok();
        }
    }
}

/// Resolves when the tunnel's cloudflared exits; never without a tunnel.
async fn tunnel_exit(t: &mut Option<tunnel::Tunnel>) {
    match t {
        Some(t) => t.exited().await,
        None => std::future::pending().await,
    }
}

/// What a client needs to show one pairing code: the link, its QR as rows of
/// `0`/`1`, and the servers it reaches.
fn code_json(url: &str, shared: &server::Shared) -> anyhow::Result<serde_json::Value> {
    let qr = qrcode::QrCode::new(url.as_bytes()).context("encoding the QR code")?;
    let w = qr.width();
    let colors = qr.to_colors();
    let rows: Vec<String> = (0..w)
        .map(|y| (0..w).map(|x| if colors[y * w + x] == qrcode::Color::Dark { '1' } else { '0' }).collect())
        .collect();
    Ok(json!({
        "url": url,
        "typing": shared.allow_typing(),
        "expires_in": CODE_TTL.as_secs(),
        "qr": {"size": w, "rows": rows},
        "gateway": lmux_core::daemon::hostname(),
        "servers": shared.server_names(),
    }))
}
