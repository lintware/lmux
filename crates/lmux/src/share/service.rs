//! The always-on gateway (`lmux share --service`), as run by the
//! `lmux-gateway` systemd user service.
//!
//! It shows no code by itself. A client that can log in to the gateway, like
//! the laptop app over SSH, asks for one with `lmux share-code`, which talks
//! to the control socket `<state_dir>/share.sock` (mode 0600). The current
//! address is also written to `<state_dir>/share.json`.
//!
//! When cloudflared exits, the service starts a new tunnel itself (a new
//! address), rewrites `share.json` and tells connected phones. Sessions live
//! in `<state_dir>/share-sessions.json`, so even a restart of the service
//! keeps its phones. `lmux share-sessions` lists them and `lmux share-revoke`
//! ends one (or all) at once.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::server::Shared;
use super::tunnel::Tunnel;
use super::Keeper;

fn control_path() -> PathBuf {
    lmux_core::default_state_dir().join("share.sock")
}

fn state_path() -> PathBuf {
    lmux_core::default_state_dir().join("share.json")
}

pub async fn run(shared: Arc<Shared>, base: String, mut tunnel: Option<Tunnel>, keeper: Keeper) -> anyhow::Result<()> {
    let dir = lmux_core::default_state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let sock = control_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

    write_state(&shared, &base)?;
    eprintln!("lmux gateway at {base} for {}", shared.server_names().join(", "));
    for url in shared.endpoints().urls.iter().filter(|u| **u != base) {
        eprintln!("also at {url}");
    }

    let accept = async {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let shared = shared.clone();
            tokio::spawn(async move {
                let _ = control(stream, &shared).await;
            });
        }
    };
    let moved = |url: &str| {
        eprintln!("the tunnel restarted at {url}; connected phones were told");
        let _ = write_state(&shared, url);
    };
    let mut gave_up = false;
    tokio::select! {
        _ = accept => {},
        r = keeper.keep(&mut tunnel, moved) => {
            if let Err(e) = r { eprintln!("{e:#}"); }
            gave_up = true;
        },
        _ = super::stop_signal() => {},
    }
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(state_path());
    let _ = std::fs::remove_dir_all(shared.uploads_dir());
    // A tunnel that cannot be started again is a failure, so systemd tries
    // again later. Sessions are on disk, so phones stay paired.
    if gave_up {
        anyhow::bail!("the tunnel stopped");
    }
    Ok(())
}

/// `share.json`: where the gateway is now, for the app and for people.
fn write_state(shared: &Shared, url: &str) -> anyhow::Result<()> {
    let state = json!({
        "url": url,
        "urls": shared.endpoints().urls,
        "pid": std::process::id(),
        "gateway": lmux_core::daemon::hostname(),
        "servers": shared.server_names(),
        "typing": shared.allow_typing(),
    });
    write_private(&state_path(), &format!("{state}\n"))
}

/// One control request: `{"op":"code"}` makes a new code,
/// `{"op":"status","code":…}` says whether that code still waits for a scan,
/// `{"op":"sessions"}` lists the paired phones and `{"op":"revoke","id":…}`
/// (or `"all":true`) ends them now.
async fn control(stream: UnixStream, shared: &Shared) -> anyhow::Result<()> {
    let (mut r, mut w) = stream.into_split();
    let mut line = String::new();
    BufReader::with_capacity(1, &mut r).read_line(&mut line).await?;
    let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    if req["op"] == "attach" {
        let name = req["name"].as_str().context("missing server name")?.to_owned();
        w.write_all(b"{\"attached\":true}\n").await?;
        return shared.attach(name, r.reunite(w)?).await;
    }
    let reply = match req["op"].as_str() {
        Some("code") => {
            // The tunnel's address now: it changes when cloudflared restarts.
            let base = shared.endpoints().primary.unwrap_or_default();
            let code = shared.new_code();
            let mut v = super::code_json(&format!("{base}/#{code}"), shared)?;
            v["code"] = json!(code);
            v
        }
        Some("status") => json!({"live": shared.code_live(req["code"].as_str().unwrap_or(""))}),
        Some("sessions") => json!({"sessions": shared.auth.list()}),
        Some("revoke") => match (req["id"].as_str(), req["all"].as_bool()) {
            (Some(id), _) => json!({"revoked": shared.auth.revoke(Some(id))}),
            (None, Some(true)) => json!({"revoked": shared.auth.revoke(None)}),
            _ => json!({"error": "revoke needs an id or all"}),
        },
        _ => json!({"error": "unknown op"}),
    };
    w.write_all(format!("{reply}\n").as_bytes()).await?;
    Ok(())
}

/// SSH-side bridge to the private, same-user gateway control socket.
pub async fn attach(name: String) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(control_path()).await.context("no lmux gateway is running here")?;
    stream.write_all(format!("{}\n", json!({"op":"attach", "name":name})).as_bytes()).await?;
    let mut reader = BufReader::new(stream);
    let mut ack = String::new();
    reader.read_line(&mut ack).await?;
    anyhow::ensure!(serde_json::from_str::<Value>(&ack)?["attached"] == true, "gateway refused backend");
    let (mut r, mut w) = reader.into_inner().into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut w) => { result?; },
        result = tokio::io::copy(&mut r, &mut stdout) => { result?; },
    }
    Ok(())
}

/// `lmux share-code [--status CODE]`: ask the running gateway service for a
/// new pairing code (printed as JSON), or whether a code is still unused.
pub async fn request_code(status: Option<String>) -> anyhow::Result<()> {
    let req = match status {
        Some(code) => json!({"op": "status", "code": code}),
        None => json!({"op": "code"}),
    };
    print!("{}", ask(&req).await?);
    Ok(())
}

/// One request to the gateway's control socket; its answer line.
async fn ask(req: &Value) -> anyhow::Result<String> {
    let sock = control_path();
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("no lmux gateway is running here ({})", sock.display()))?;
    let (r, mut w) = stream.into_split();
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    Ok(line)
}

/// `lmux share-sessions`: the phones paired with the gateway service here.
pub async fn sessions(json_out: bool) -> anyhow::Result<()> {
    let v: Value = serde_json::from_str(&ask(&json!({"op": "sessions"})).await?).context("gateway answer")?;
    if json_out {
        println!("{v}");
        return Ok(());
    }
    let list = v["sessions"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No paired phones.");
    }
    let ago = |t: &Value| {
        let s = super::auth::now().saturating_sub(t.as_u64().unwrap_or(0));
        if s < 3600 { format!("{}m ago", s / 60) } else if s < 86400 { format!("{}h ago", s / 3600) } else { format!("{}d ago", s / 86400) }
    };
    for s in list {
        println!("{}  paired {}  last used {}", s["id"].as_str().unwrap_or("?"), ago(&s["created"]), ago(&s["last_used"]));
    }
    Ok(())
}

/// `lmux share-revoke ID|--all`: end paired phones' sessions now. Their open
/// sockets close within a second.
pub async fn revoke(id: Option<String>, all: bool) -> anyhow::Result<()> {
    let req = match (id, all) {
        (Some(id), false) => json!({"op": "revoke", "id": id}),
        (None, true) => json!({"op": "revoke", "all": true}),
        _ => anyhow::bail!("give a session id from `lmux share-sessions`, or --all"),
    };
    let v: Value = serde_json::from_str(&ask(&req).await?).context("gateway answer")?;
    if let Some(e) = v["error"].as_str() {
        anyhow::bail!("{e}");
    }
    let n = v["revoked"].as_u64().unwrap_or(0);
    println!("Revoked {n} session{}.", if n == 1 { "" } else { "s" });
    if n == 0 {
        anyhow::bail!("no such session");
    }
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
