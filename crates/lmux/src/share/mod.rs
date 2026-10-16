//! `lmux share`: serve the session list and live terminals to a browser, and
//! reach the laptop through a Cloudflare Quick Tunnel.
//!
//! A terminal is remote code execution, so the rules are strict: the link
//! carries a single-use code in the URL fragment (never sent to a server),
//! the page trades it for an `HttpOnly` cookie that lapses when unused, the WebSocket
//! checks `Origin` and that cookie, and typing is off unless asked for.
//! Screen contents are never logged.
//!
//! A share should not end out from under a phone that keeps using it:
//! credentials roll over on the live socket (`auth`), the tunnel is started
//! again when cloudflared exits, and connected clients hear the new address.
//! On a tailnet the share also listens at a fixed tailnet address
//! (`tailnet`), which phones on the tailnet prefer.

mod agents;
mod auth;
mod host;
mod remote;
mod server;
mod service;
mod tailnet;
mod tunnel;
mod upload;
mod voice;
mod voice_tools;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde_json::json;

use auth::CODE_TTL;

/// Starts of cloudflared in a row that may fail before the share gives up.
const TUNNEL_TRIES: u32 = 6;

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
pub use service::{revoke, sessions};

pub async fn run(opts: Opts) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port))
        .await
        .context("binding the local share port")?;
    let port = listener.local_addr()?.port();
    // The gateway service keeps its phones across restarts; a share the app
    // or a terminal runs ends every session when it stops.
    let auth = if opts.service {
        let dir = lmux_core::default_state_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        auth::Auth::persistent(dir.join("share-sessions.json"))
    } else {
        auth::Auth::in_memory()
    };
    let shared = Arc::new(server::Shared::with_servers(opts.socket.clone(), opts.servers.clone(), opts.allow_typing, auth));
    tokio::spawn(server::serve(shared.clone(), listener, true));

    // The tailnet address: same server, plain HTTP inside WireGuard.
    let tailnet_urls = if opts.no_tunnel { Vec::new() } else { listen_on_tailnet(&shared).await };

    // Keep the tunnel handle alive: dropping it kills cloudflared.
    let (base, tunnel) = if opts.no_tunnel {
        (format!("http://127.0.0.1:{port}"), None)
    } else {
        if !opts.json {
            eprintln!("Starting a Cloudflare Quick Tunnel…");
        }
        let t = tunnel::start(port, opts.cloudflared.as_deref(), !opts.json, opts.service).await?;
        (t.url.clone(), Some(t))
    };
    shared.set_endpoints(Some(base.clone()), endpoint_urls(&tailnet_urls, &base));
    let keeper = Keeper {
        shared: shared.clone(),
        port,
        cloudflared: opts.cloudflared.clone(),
        // `--json`: the app stops reading after the first line, so a later
        // write to stdout or stderr would end the share with SIGPIPE.
        print: !opts.json,
        service: opts.service,
        tailnet: tailnet_urls,
    };

    if opts.service {
        return service::run(shared, base, tunnel, keeper).await;
    }
    let mut tunnel = tunnel;

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
        tokio::select! { _ = closed => {}, _ = stop_signal() => {}, _ = keeper.keep(&mut tunnel, |_| {}) => {} }
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
    println!("Scan the code or open the link on your phone within {} minutes; it works once.", CODE_TTL / 60);
    if !opts.no_tunnel {
        println!(
            "Quick Tunnel limits: the address changes every start, there is no uptime guarantee, \
             and Cloudflare answers 429 above 200 requests in flight. When it restarts, connected \
             phones are told the new address."
        );
    }
    if let Some(t) = keeper.tailnet.first() {
        println!("Tailnet: {t} (phones on the tailnet use it first).");
    }
    println!("Press Ctrl-C to stop sharing.");
    let shared2 = shared.clone();
    let moved = move |url: &str| {
        let code = shared2.new_code();
        eprintln!("The tunnel restarted. Connected phones follow; a new phone can use {url}/#{code}");
    };
    tokio::select! { _ = stop_signal() => {}, r = keeper.keep(&mut tunnel, moved) => {
        if let Err(e) = r { eprintln!("{e:#}"); }
    } }
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

/// Every address of the share, the ones clients should try first first: the
/// tailnet's (stable), then the tunnel's (public).
fn endpoint_urls(tailnet: &[String], tunnel: &str) -> Vec<String> {
    let mut urls = tailnet.to_vec();
    urls.push(tunnel.to_string());
    urls
}

/// Listen at this machine's tailnet address too, when it has one. Returns
/// the URLs it serves there.
async fn listen_on_tailnet(shared: &Arc<server::Shared>) -> Vec<String> {
    let Some(t) = tailnet::detect().await else { return Vec::new() };
    let listener = match tokio::net::TcpListener::bind((t.ip, tailnet::PORT)).await {
        Ok(l) => l,
        Err(_) => match tokio::net::TcpListener::bind((t.ip, 0)).await {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        },
    };
    let Ok(addr) = listener.local_addr() else { return Vec::new() };
    tokio::spawn(server::serve(shared.clone(), listener, false));
    t.urls(addr.port())
}

/// Keeps the Quick Tunnel up: when cloudflared exits, starts another (it
/// gets a new address) and records it, so connected clients hear of it.
pub(crate) struct Keeper {
    shared: Arc<server::Shared>,
    port: u16,
    cloudflared: Option<String>,
    print: bool,
    service: bool,
    tailnet: Vec<String>,
}

impl Keeper {
    /// Runs until the tunnel cannot be started again (`TUNNEL_TRIES` in a
    /// row); never returns without a tunnel. `moved` hears each new address.
    async fn keep(&self, tunnel: &mut Option<tunnel::Tunnel>, moved: impl Fn(&str)) -> anyhow::Result<()> {
        if tunnel.is_none() {
            return std::future::pending().await;
        }
        loop {
            if let Some(t) = tunnel.as_mut() {
                t.exited().await;
            }
            *tunnel = None;
            if self.print {
                eprintln!("The tunnel stopped; starting a new one.");
            }
            let mut tries = 0;
            let t = loop {
                tries += 1;
                match tunnel::start(self.port, self.cloudflared.as_deref(), false, self.service).await {
                    Ok(t) => break t,
                    Err(e) if tries >= TUNNEL_TRIES => return Err(e.context("the tunnel could not be started again")),
                    Err(_) => tokio::time::sleep(Duration::from_secs(2u64.pow(tries).min(60))).await,
                }
            };
            self.shared.set_endpoints(Some(t.url.clone()), endpoint_urls(&self.tailnet, &t.url));
            moved(&t.url);
            *tunnel = Some(t);
        }
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
        "expires_in": CODE_TTL,
        "qr": {"size": w, "rows": rows},
        "gateway": lmux_core::daemon::hostname(),
        "servers": shared.server_names(),
    }))
}
