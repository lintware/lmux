//! `lmux share`: serve the session list and live terminals to a browser, and
//! reach the laptop through a Cloudflare Quick Tunnel.
//!
//! A terminal is remote code execution, so the rules are strict: the link
//! carries a single-use code in the URL fragment (never sent to a server),
//! the page trades it for a short-lived `HttpOnly` cookie, the WebSocket
//! checks `Origin` and that cookie, and typing is off unless asked for.
//! Screen contents are never logged.

mod server;
mod tunnel;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde_json::json;

/// How long an unused code works.
const CODE_TTL: Duration = Duration::from_secs(300);
/// How long a browser session (cookie) lasts.
const SESSION_TTL: Duration = Duration::from_secs(3600);

pub struct Opts {
    pub socket: String,
    pub allow_typing: bool,
    pub port: u16,
    pub no_tunnel: bool,
    pub cloudflared: Option<String>,
    pub json: bool,
}

pub async fn run(opts: Opts) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port))
        .await
        .context("binding the local share port")?;
    let port = listener.local_addr()?.port();
    let shared = Arc::new(server::Shared::new(opts.socket.clone(), opts.allow_typing));
    let code = shared.new_code();
    tokio::spawn(server::serve(shared.clone(), listener));

    // Keep the tunnel handle alive: dropping it kills cloudflared.
    let (base, _tunnel) = if opts.no_tunnel {
        (format!("http://127.0.0.1:{port}"), None)
    } else {
        if !opts.json {
            eprintln!("Starting a Cloudflare Quick Tunnel…");
        }
        let t = tunnel::start(port, opts.cloudflared.as_deref(), !opts.json).await?;
        (t.url.clone(), Some(t))
    };
    let url = format!("{base}/#{code}");
    let qr = qrcode::QrCode::new(url.as_bytes()).context("encoding the QR code")?;

    if opts.json {
        let w = qr.width();
        let colors = qr.to_colors();
        let rows: Vec<String> = (0..w)
            .map(|y| {
                (0..w)
                    .map(|x| if colors[y * w + x] == qrcode::Color::Dark { '1' } else { '0' })
                    .collect()
            })
            .collect();
        println!(
            "{}",
            json!({
                "url": url,
                "typing": opts.allow_typing,
                "expires_in": CODE_TTL.as_secs(),
                "qr": {"size": w, "rows": rows},
            })
        );
        // The app owns this process: when it closes our stdin or exits,
        // sharing ends.
        let mut sink = [0u8; 64];
        use tokio::io::AsyncReadExt;
        let mut stdin = tokio::io::stdin();
        let closed = async { while matches!(stdin.read(&mut sink).await, Ok(n) if n > 0) {} };
        tokio::select! { _ = closed => {}, _ = stop_signal() => {} }
        return Ok(());
    }

    use qrcode::render::unicode::Dense1x2;
    println!("{}", qr.render::<Dense1x2>().quiet_zone(true).build());
    println!("{url}\n");
    println!(
        "Mode: {}.",
        if opts.allow_typing { "typing allowed" } else { "view only (add --allow-typing to type)" }
    );
    println!("Scan the code or open the link on your phone within {} minutes; it works once.", CODE_TTL.as_secs() / 60);
    if !opts.no_tunnel {
        println!(
            "Quick Tunnel limits: the address changes every start, there is no uptime guarantee, \
             and Cloudflare answers 429 above 200 requests in flight."
        );
    }
    println!("Press Ctrl-C to stop sharing.");
    stop_signal().await;
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
