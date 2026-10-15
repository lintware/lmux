//! A server the gateway controls over SSH.
//!
//! The gateway runs `lmux share-backend` on the server through `ssh` and
//! talks to it with one JSON object per line: requests `{"id","op",...}`,
//! answers `{"id","ok"}` or `{"id","err"}`. The backend's first line is
//! `{"hello":1,"host"}`. When SSH drops, the connection is made again with a
//! growing pause, and calls fail fast meanwhile.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use super::host::{Local, TREE_TTL};
use super::upload::Pending;

/// SSH runs a non-login shell, whose PATH may lack `~/.local/bin`
/// (where `deploy/install-remote.sh` puts lmux) or Homebrew.
const REMOTE_PATH: &str = "export PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\";";

/// The command run on the server. `LMUX_REMOTE_BIN` names another lmux
/// binary there (a test build, or one outside PATH).
fn remote_cmd() -> String {
    let bin = remote_bin();
    format!("{REMOTE_PATH} exec {bin} share-backend")
}

fn remote_bin() -> String {
    std::env::var("LMUX_REMOTE_BIN").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "lmux".into())
}
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Uploads carry up to 25 MB through SSH.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Connected,
    Unreachable,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Connecting => "connecting",
            Status::Connected => "connected",
            Status::Unreachable => "unreachable",
        }
    }
}

type Waiters = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct Link {
    tx: mpsc::UnboundedSender<String>,
    waiters: Waiters,
}

pub struct Remote {
    /// The SSH target, e.g. an alias from `~/.ssh/config`.
    pub target: String,
    state: Mutex<(Status, Option<String>)>,
    link: Mutex<Option<Arc<Link>>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Remote {
    pub fn attached(target: String) -> Arc<Self> {
        Arc::new(Self { target, state: Mutex::new((Status::Connecting, None)), link: Mutex::new(None), next_id: 1.into() })
    }

    pub async fn serve_attached(&self, stream: tokio::net::UnixStream) -> anyhow::Result<()> {
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        let result = async {
            let hello = tokio::time::timeout(Duration::from_secs(20), lines.next_line()).await??.context("no backend hello")?;
            let hello: Value = serde_json::from_str(&hello)?;
            anyhow::ensure!(hello["hello"] == 1, "unsupported backend protocol");
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            let waiters: Waiters = Arc::default();
            *self.link.lock().unwrap() = Some(Arc::new(Link { tx, waiters: waiters.clone() }));
            self.set(Status::Connected, None);
            let writer = async {
                while let Some(line) = rx.recv().await {
                    w.write_all(format!("{line}\n").as_bytes()).await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            let reader = async {
                while let Some(line) = lines.next_line().await? {
                    let v: Value = serde_json::from_str(&line)?;
                    if let Some(id) = v["id"].as_u64() {
                        if let Some(tx) = waiters.lock().unwrap().remove(&id) {
                            let reply = match v["err"].as_str() {
                                Some(e) => Err(e.to_owned()),
                                None => Ok(v["ok"].clone()),
                            };
                            let _ = tx.send(reply);
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            };
            let result = tokio::select! { r = writer => r, r = reader => r };
            waiters.lock().unwrap().clear();
            result
        }.await;
        *self.link.lock().unwrap() = None;
        self.set(Status::Unreachable, Some("the laptop connection closed".into()));
        result
    }

    /// Starts connecting in the background and keeps the connection up.
    pub fn start(target: String) -> Arc<Remote> {
        let r = Arc::new(Remote {
            target,
            state: Mutex::new((Status::Connecting, None)),
            link: Mutex::new(None),
            next_id: 1.into(),
        });
        tokio::spawn(supervise(r.clone()));
        r
    }

    pub fn status(&self) -> (Status, Option<String>) {
        self.state.lock().unwrap().clone()
    }

    fn set(&self, s: Status, err: Option<String>) {
        *self.state.lock().unwrap() = (s, err);
    }

    pub async fn call(&self, mut req: Value) -> anyhow::Result<Value> {
        let link = self.link.lock().unwrap().clone().ok_or_else(|| anyhow!("{} is not connected", self.target))?;
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let limit = if req["op"] == "upload" { UPLOAD_TIMEOUT } else { CALL_TIMEOUT };
        req["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        link.waiters.lock().unwrap().insert(id, tx);
        if link.tx.send(req.to_string()).is_err() {
            link.waiters.lock().unwrap().remove(&id);
            bail!("{} is not connected", self.target);
        }
        match tokio::time::timeout(limit, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(e)),
            Ok(Err(_)) => bail!("the connection to {} dropped", self.target),
            Err(_) => {
                link.waiters.lock().unwrap().remove(&id);
                bail!("{} did not answer", self.target)
            }
        }
    }
}

async fn supervise(r: Arc<Remote>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        // Keep the last failure while trying again, so it can be shown.
        let last = r.status().1;
        r.set(Status::Connecting, last);
        let started = std::time::Instant::now();
        let err = match connect_once(&r).await {
            Ok(()) => "the connection closed".to_string(),
            Err(e) => format!("{e:#}"),
        };
        *r.link.lock().unwrap() = None;
        r.set(Status::Unreachable, Some(err));
        // A connection that held for a while starts the pauses over.
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One SSH session: returns when it ends.
async fn connect_once(r: &Arc<Remote>) -> anyhow::Result<()> {
    let mut child = Command::new("ssh")
        .env_remove("OPENAI_API_KEY")
        .args(["-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"])
        .args(["-o", "ConnectTimeout=10"])
        .arg(&r.target)
        .arg(remote_cmd())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ssh")?;
    let mut stdin = child.stdin.take().context("ssh stdin")?;
    let mut lines = BufReader::new(child.stdout.take().context("ssh stdout")?).lines();
    // Keep the last stderr line: it says why ssh or the backend failed.
    let last_err = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let last_err = last_err.clone();
        tokio::spawn(async move {
            let mut l = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = l.next_line().await {
                if !line.trim().is_empty() {
                    *last_err.lock().unwrap() = line;
                }
            }
        });
    }
    let failed = |what: &str| {
        let e = last_err.lock().unwrap().clone();
        if e.contains("unrecognized subcommand") {
            anyhow!("lmux on this server is too old; update it with deploy/install-remote.sh")
        } else if e.is_empty() {
            anyhow!("{what}")
        } else {
            anyhow!("{what}: {e}")
        }
    };

    let hello = tokio::time::timeout(Duration::from_secs(20), lines.next_line()).await;
    let hello: Value = match hello {
        Ok(Ok(Some(l))) => serde_json::from_str(&l).map_err(|_| failed("the server sent no hello"))?,
        Ok(_) => {
            // Let stderr catch up before naming the cause.
            tokio::time::sleep(Duration::from_millis(200)).await;
            return Err(failed("ssh ended"));
        }
        Err(_) => return Err(failed("no answer within 20 s")),
    };
    if hello["hello"] != 1 {
        bail!("the server's lmux speaks another protocol; update it with deploy/install-remote.sh");
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let waiters: Waiters = Arc::default();
    *r.link.lock().unwrap() = Some(Arc::new(Link { tx, waiters: waiters.clone() }));
    r.set(Status::Connected, None);

    let writer = async {
        while let Some(mut line) = rx.recv().await {
            line.push('\n');
            if stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    };
    let reader = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let Some(id) = v["id"].as_u64() else { continue };
            if let Some(w) = waiters.lock().unwrap().remove(&id) {
                let res = match v.get("err").and_then(Value::as_str) {
                    Some(e) => Err(e.to_string()),
                    None => Ok(v["ok"].clone()),
                };
                let _ = w.send(res);
            }
        }
    };
    tokio::select! { _ = writer => {}, _ = reader => {}, _ = child.wait() => {} }
    // Dropping the senders fails every call still waiting.
    waiters.lock().unwrap().clear();
    Err(failed("the connection closed"))
}

/// `lmux share-backend`: serve this machine's sessions to a gateway over
/// stdin and stdout. The gateway authenticates the viewer; this side trusts
/// whoever can run it over SSH, like the daemon's own socket.
pub async fn backend(socket: String) -> anyhow::Result<()> {
    backend_io(socket, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn backend_io<R, W>(socket: String, input: R, mut out: W) -> anyhow::Result<()>
where R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin + Send + 'static {
    let local = Arc::new(Local::new(socket));
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let hello = json!({"hello": 1, "host": lmux_core::daemon::hostname(), "version": env!("CARGO_PKG_VERSION")});
    out.write_all(format!("{hello}\n").as_bytes()).await?;
    out.flush().await?;
    let writer = async move {
        while let Some(mut line) = rx.recv().await {
            line.push('\n');
            if out.write_all(line.as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    };
    let reader_local = local.clone();
    let reader = async move {
        let mut lines = BufReader::new(input).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
            let (local, tx) = (reader_local.clone(), tx.clone());
            tokio::spawn(async move {
                let id = req["id"].clone();
                let reply = match answer(&local, &req).await {
                    Ok(v) => json!({"id": id, "ok": v}),
                    Err(e) => json!({"id": id, "err": format!("{e:#}")}),
                };
                let _ = tx.send(reply.to_string());
            });
        }
    };
    tokio::pin!(writer);
    tokio::select! { _ = &mut writer => {}, _ = reader => { writer.await; } }
    let _ = std::fs::remove_dir_all(local.uploads_dir());
    Ok(())
}

/// The laptop initiates SSH, then answers the gateway's backend requests.
pub async fn connect_gateway(socket: String, gateway: String) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let closed = async {
        let mut stdin = tokio::io::stdin();
        let mut bytes = [0; 64];
        while matches!(stdin.read(&mut bytes).await, Ok(n) if n > 0) {}
    };
    tokio::select! {
        result = connect_gateway_loop(socket, gateway) => result,
        _ = closed => Ok(()),
        _ = super::stop_signal() => Ok(()),
    }
}

async fn connect_gateway_loop(socket: String, gateway: String) -> anyhow::Result<()> {
    let name = lmux_core::daemon::hostname();
    anyhow::ensure!(!name.contains('\'') && !name.contains('/'), "invalid hostname");
    loop {
        let mut child = Command::new("ssh")
            .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"])
            .arg("--").arg(&gateway).arg(format!("{REMOTE_PATH} exec {} share-attach --name '{name}'", remote_bin()))
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true).spawn()?;
        let input = child.stdout.take().context("ssh stdout")?;
        let output = child.stdin.take().context("ssh stdin")?;
        if let Err(e) = backend_io(socket.clone(), input, output).await {
            eprintln!("gateway backend: {e:#}");
        }
        let _ = child.kill().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn answer(local: &Local, req: &Value) -> anyhow::Result<Value> {
    let surface = || req["surface"].as_str().filter(|s| super::server::valid_surface(s)).context("bad tab");
    match req["op"].as_str().unwrap_or("") {
        "tree" => local.tree().await,
        "screen" => Ok(json!({"text": local.screen(surface()?).await?})),
        "chat" => Ok(match local.chat(surface()?, req["stamp"].as_str()).await {
            Some((stamp, chat)) => json!({"stamp": stamp, "chat": chat}),
            None => Value::Null,
        }),
        "notifications" => Ok(json!({"notifications": local.notifications().await?})),
        "send" => {
            let text = req["text"].as_str().unwrap_or("");
            if text.len() > super::server::MAX_TEXT + 1 {
                bail!("text too long");
            }
            local.send(surface()?, text).await.map(|_| Value::Null)
        }
        "key" => {
            let key = req["key"].as_str().filter(|k| super::server::KEYS.contains(k)).context("unknown key")?;
            local.key(surface()?, key).await.map(|_| Value::Null)
        }
        "upload" => {
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD
                .decode(req["data"].as_str().unwrap_or(""))
                .context("bad upload data")?;
            let p = Pending::whole(req["name"].as_str().unwrap_or(""), data).map_err(|e| anyhow!(e))?;
            Ok(json!({"path": local.save(p).await?}))
        }
        "ping" => Ok(json!({"tree_ttl_ms": TREE_TTL.as_millis() as u64})),
        other => bail!("unknown op {other:?}"),
    }
}
