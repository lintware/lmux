//! The always-on gateway (`lmux share --service`), as run by the
//! `lmux-gateway` systemd user service.
//!
//! It shows no code by itself. A client that can log in to the gateway, like
//! the laptop app over SSH, asks for one with `lmux share-code`, which talks
//! to the control socket `<state_dir>/share.sock` (mode 0600). The current
//! address is also written to `<state_dir>/share.json`.
//!
//! When cloudflared exits the address is dead, so the service exits too and
//! systemd starts it again with a new address.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::server::Shared;
use super::tunnel::Tunnel;

fn control_path() -> PathBuf {
    lmux_core::default_state_dir().join("share.sock")
}

fn state_path() -> PathBuf {
    lmux_core::default_state_dir().join("share.json")
}

pub async fn run(shared: Arc<Shared>, base: String, mut tunnel: Option<Tunnel>) -> anyhow::Result<()> {
    let dir = lmux_core::default_state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let sock = control_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

    let state = json!({
        "url": base,
        "pid": std::process::id(),
        "gateway": lmux_core::daemon::hostname(),
        "servers": shared.server_names(),
        "typing": shared.allow_typing(),
    });
    write_private(&state_path(), &format!("{state}\n"))?;
    eprintln!("lmux gateway at {base} for {}", shared.server_names().join(", "));

    let accept = async {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let (shared, base) = (shared.clone(), base.clone());
            tokio::spawn(async move {
                let _ = control(stream, &shared, &base).await;
            });
        }
    };
    let died = async {
        match tunnel.as_mut() {
            Some(t) => {
                t.exited().await;
                eprintln!("the tunnel stopped; exiting so the service restarts with a new one");
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! { _ = accept => {}, _ = died => {}, _ = super::stop_signal() => {} }
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(state_path());
    let _ = std::fs::remove_dir_all(shared.uploads_dir());
    // A dead tunnel is a failure, so `Restart=on-failure` brings it back.
    if tunnel.is_some() {
        anyhow::bail!("the tunnel stopped");
    }
    Ok(())
}

/// One control request: `{"op":"code"}` makes a new code, and
/// `{"op":"status","code":…}` says whether that code still waits for a scan.
async fn control(stream: UnixStream, shared: &Shared, base: &str) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    let reply = match req["op"].as_str() {
        Some("code") => {
            let code = shared.new_code();
            let mut v = super::code_json(&format!("{base}/#{code}"), shared)?;
            v["code"] = json!(code);
            v
        }
        Some("status") => json!({"live": shared.code_live(req["code"].as_str().unwrap_or(""))}),
        _ => json!({"error": "unknown op"}),
    };
    w.write_all(format!("{reply}\n").as_bytes()).await?;
    Ok(())
}

/// `lmux share-code [--status CODE]`: ask the running gateway service for a
/// new pairing code (printed as JSON), or whether a code is still unused.
pub async fn request_code(status: Option<String>) -> anyhow::Result<()> {
    let sock = control_path();
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("no lmux gateway is running here ({})", sock.display()))?;
    let (r, mut w) = stream.into_split();
    let req = match status {
        Some(code) => json!({"op": "status", "code": code}),
        None => json!({"op": "code"}),
    };
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    print!("{line}");
    Ok(())
}

fn write_private(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    std::io::Write::write_all(&mut f, text.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
