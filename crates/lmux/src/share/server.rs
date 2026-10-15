//! The share server: a few static files, the code-for-cookie exchange, and
//! one WebSocket that carries the session list, screens and keystrokes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use super::host::{Host, Local, TREE_TTL};
use super::remote::Remote;
use super::upload::{Pending, Step};
use super::voice::{OfferError, PageCmd, Voice, VoiceConfig, MAX_SDP};
use super::voice_tools::Backend;
use super::{CODE_TTL, SESSION_IDLE, SESSION_MAX};

const PAGE: &str = include_str!("page.html");
const JS: &str = include_str!("app.js");
const CSS: &str = include_str!("app.css");

const COOKIE: &str = "lmux_session";
const MAX_HEADERS: usize = 16 * 1024;
const MAX_CLIENTS: usize = 8;
pub const MAX_TEXT: usize = 4096;
pub const KEYS: &[&str] = &[
    "enter", "escape", "esc", "tab", "up", "down", "left", "right", "backspace", "ctrl+c", "ctrl+d",
];

/// One server whose sessions this share serves. Ids of a remote server's
/// workspaces and tabs carry `<name>/` in front, so every action finds its
/// server; this machine's ids stay bare (`surface:3`).
pub struct Server {
    pub name: String,
    pub host: Host,
}

impl Server {
    fn is_local(&self) -> bool {
        matches!(self.host, Host::Local(_))
    }

    /// `connected`, `connecting` or `unreachable`, and why when not connected.
    fn status(&self) -> (&'static str, Option<String>) {
        match &self.host {
            Host::Local(_) => ("connected", None),
            Host::Remote(r) => {
                let (s, e) = r.status();
                (s.label(), e)
            }
        }
    }
}

/// When a session token stops working: after `idle` passes with no use, or at
/// `end` regardless.
struct Session {
    idle: Instant,
    end: Instant,
}

pub struct Shared {
    local: Arc<Local>,
    servers: Vec<Server>,
    /// True when other servers are controlled too (gateway mode).
    gateway: bool,
    allow_typing: bool,
    codes: Mutex<HashMap<String, Instant>>,
    sessions: Mutex<HashMap<String, Session>>,
    clients: Mutex<usize>,
    /// The last merged tree message, shared by every socket for `TREE_TTL`.
    tree: tokio::sync::Mutex<Option<(Instant, String)>>,
    /// The voice call, when a key is set up (and `LMUX_VOICE` is not `0`).
    voice: Option<Arc<Voice>>,
}

impl Shared {
    #[cfg(test)]
    pub fn new(socket: String, allow_typing: bool) -> Self {
        Self::with_servers(socket, Vec::new(), allow_typing)
    }

    /// This machine plus the servers reached over SSH at `targets`.
    pub fn with_servers(socket: String, targets: Vec<String>, allow_typing: bool) -> Self {
        let local = Arc::new(Local::new(socket));
        let mut servers = vec![Server { name: lmux_core::daemon::hostname(), host: Host::Local(local.clone()) }];
        let gateway = !targets.is_empty();
        for t in targets {
            servers.push(Server { name: t.clone(), host: Host::Remote(Remote::start(t)) });
        }
        Shared {
            local,
            servers,
            gateway,
            allow_typing,
            codes: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            clients: Mutex::new(0),
            tree: tokio::sync::Mutex::new(None),
            voice: voice_config().map(|c| Arc::new(Voice::new(c))),
        }
    }

    /// Which credential the Talk button uses, when voice is on.
    pub fn voice_label(&self) -> Option<&'static str> {
        self.voice.as_ref().map(|v| v.label())
    }

    pub fn uploads_dir(&self) -> &std::path::Path {
        self.local.uploads_dir()
    }

    pub fn new_code(&self) -> String {
        let code = random_token();
        self.codes.lock().unwrap().insert(code.clone(), Instant::now() + CODE_TTL);
        code
    }

    /// Whether `code` is still waiting to be scanned.
    pub fn code_live(&self, code: &str) -> bool {
        let mut codes = self.codes.lock().unwrap();
        let now = Instant::now();
        codes.retain(|_, exp| *exp > now);
        codes.keys().any(|k| ct_eq(k.as_bytes(), code.as_bytes()))
    }

    pub fn allow_typing(&self) -> bool {
        self.allow_typing
    }

    /// The names of the servers this share controls, this machine first.
    pub fn server_names(&self) -> Vec<String> {
        self.servers.iter().map(|s| s.name.clone()).collect()
    }

    /// Spend a code. Returns a session token when the code was live.
    fn redeem(&self, code: &str) -> Option<String> {
        let mut codes = self.codes.lock().unwrap();
        let now = Instant::now();
        codes.retain(|_, exp| *exp > now);
        // Look every stored code up in constant time per entry.
        let hit = codes.keys().find(|k| ct_eq(k.as_bytes(), code.as_bytes())).cloned();
        let hit = hit?;
        codes.remove(&hit);
        drop(codes);
        let token = random_token();
        self.sessions.lock().unwrap().insert(token.clone(), Session { idle: now + SESSION_IDLE, end: now + SESSION_MAX });
        Some(token)
    }

    /// Whether `token` is a live session, and if so keep it alive for another
    /// `SESSION_IDLE` (never past its `SESSION_MAX`).
    fn session_alive(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        s.retain(|_, v| v.idle > now && v.end > now);
        match s.iter_mut().find(|(k, _)| ct_eq(k.as_bytes(), token.as_bytes())) {
            Some((_, v)) => {
                v.idle = (now + SESSION_IDLE).min(v.end);
                true
            }
            None => false,
        }
    }

    /// The server that owns tab `id`, and the tab's id on that server.
    fn route<'a>(&self, id: &'a str) -> Option<(&Server, &'a str)> {
        let (server, surface) = match id.split_once('/') {
            Some((name, s)) => (self.servers.iter().find(|v| !v.is_local() && v.name == name)?, s),
            None => (self.servers.first()?, id),
        };
        valid_surface(surface).then_some((server, surface))
    }
}

fn voice_config() -> Option<VoiceConfig> {
    if std::env::var("LMUX_VOICE").is_ok_and(|v| v == "0") {
        return None;
    }
    VoiceConfig::load()
}

/// What the voice tools read: this share's tree, screens, transcripts and
/// every server's notifications.
struct ShareBackend(Arc<Shared>);

#[async_trait::async_trait]
impl Backend for ShareBackend {
    async fn tree(&self) -> anyhow::Result<Value> {
        Ok(serde_json::from_str(&tree_message(&self.0).await?)?)
    }

    async fn screen(&self, surface: &str) -> anyhow::Result<String> {
        let (server, s) = self.0.route(surface).context("No such tab. Use an id from list_sessions.")?;
        server.host.screen(s).await.map_err(|e| anyhow::anyhow!("{}: {e}", server.name))
    }

    async fn chat(&self, surface: &str) -> anyhow::Result<Option<Value>> {
        let (server, s) = self.0.route(surface).context("No such tab. Use an id from list_sessions.")?;
        Ok(server.host.chat(s, None).await.map(|(_, chat)| chat))
    }

    async fn notifications(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let all = futures_util::future::join_all(self.0.servers.iter().map(|s| s.host.notifications())).await;
        // A server that cannot answer (an old lmux, say) just has none.
        Ok(self.0.servers.iter().zip(all).flat_map(|(s, r)| r.unwrap_or_default().into_iter().map(|n| (s.name.clone(), n))).collect())
    }
}

pub fn random_token() -> String {
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .expect("the system random source is readable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn serve(shared: Arc<Shared>, listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let shared = shared.clone();
        tokio::spawn(async move {
            // Errors are one client's problem; frames and bodies are never logged.
            let _ = tokio::time::timeout(Duration::from_secs(3 * 3600), handle(shared, stream)).await;
        });
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?
            .split(';')
            .filter_map(|p| p.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }
}

fn parse_head(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(Request { method, path, headers })
}

/// Read the request head without consuming it, so a WebSocket upgrade can still
/// be handed whole to the handshake.
async fn peek_head(stream: &TcpStream) -> anyhow::Result<(Request, usize)> {
    let mut buf = vec![0u8; MAX_HEADERS];
    loop {
        stream.readable().await?;
        let n = stream.peek(&mut buf).await?;
        if n == 0 {
            bail!("closed");
        }
        if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&buf[..end]).context("bad head")?;
            return Ok((parse_head(head).context("bad request")?, end + 4));
        }
        if n == MAX_HEADERS {
            bail!("head too large");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn respond(status: &str, ctype: &str, extra: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut h = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    let mut out = h.into_bytes();
    out.extend_from_slice(body);
    out
}

async fn handle(shared: Arc<Shared>, mut stream: TcpStream) -> anyhow::Result<()> {
    let (req, head_len) = tokio::time::timeout(Duration::from_secs(10), peek_head(&stream)).await??;
    let path = req.path.split('?').next().unwrap_or("").to_string();

    if path == "/ws" {
        return handle_ws(shared, stream, req).await;
    }

    // Consume the head (and a small body) for plain requests.
    let mut head = vec![0u8; head_len];
    stream.read_exact(&mut head).await?;
    let mut body = Vec::new();
    if let Some(len) = req.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        let limit = if path == "/voice/offer" { MAX_SDP + 256 } else { 1024 };
        if len > limit {
            stream.write_all(&respond("413 Payload Too Large", "text/plain", &[], b"too large")).await?;
            return Ok(());
        }
        body.resize(len, 0);
        stream.read_exact(&mut body).await?;
    }

    let out = match (req.method.as_str(), path.as_str()) {
        ("GET", "/") => respond("200 OK", "text/html; charset=utf-8", &[], PAGE.as_bytes()),
        ("GET", "/app.js") => respond("200 OK", "text/javascript; charset=utf-8", &[], JS.as_bytes()),
        ("GET", "/app.css") => respond("200 OK", "text/css; charset=utf-8", &[], CSS.as_bytes()),
        ("POST", "/auth") => {
            // Same-origin only: a cross-site page cannot spend a code it sees.
            if !origin_ok(&req) {
                respond("403 Forbidden", "text/plain", &[], b"bad origin")
            } else {
                let code = serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|v| v["code"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                match shared.redeem(&code) {
                    Some(token) => {
                        let cookie = format!(
                            "{COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}",
                            SESSION_MAX.as_secs()
                        );
                        respond("204 No Content", "text/plain", &[("Set-Cookie", cookie)], b"")
                    }
                    None => respond("401 Unauthorized", "text/plain", &[], b"expired"),
                }
            }
        }
        ("POST", "/voice/offer" | "/voice/talk" | "/voice/end") => voice_route(&shared, &req, &path, &body).await,
        _ => respond("404 Not Found", "text/plain", &[], b"not found"),
    };
    stream.write_all(&out).await?;
    stream.shutdown().await.ok();
    Ok(())
}

fn json_reply(status: &str, v: Value) -> Vec<u8> {
    respond(status, "application/json", &[], v.to_string().as_bytes())
}

/// The voice endpoints. Same checks as the WebSocket: the cookie and an
/// Origin that names this host. Bodies and SDP are never logged.
async fn voice_route(shared: &Arc<Shared>, req: &Request, path: &str, body: &[u8]) -> Vec<u8> {
    if !origin_ok(req) {
        return json_reply("403 Forbidden", json!({"error": "Bad origin."}));
    }
    if !req.cookie(COOKIE).is_some_and(|t| shared.session_alive(t)) {
        return json_reply("401 Unauthorized", json!({"error": "Sign in again."}));
    }
    let Some(voice) = &shared.voice else {
        return json_reply("404 Not Found", json!({"error": "Voice is not set up on this share."}));
    };
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match path {
        "/voice/offer" => {
            let Some(sdp) = v["sdp"].as_str() else {
                return json_reply("400 Bad Request", json!({"error": "An SDP offer is required."}));
            };
            match voice.offer(Arc::new(ShareBackend(shared.clone())), sdp).await {
                Ok(answer) => json_reply("201 Created", json!({"sdp": answer})),
                Err(OfferError::Busy(m)) => json_reply("429 Too Many Requests", json!({"error": m})),
                Err(OfferError::Bad(m)) => json_reply("400 Bad Request", json!({"error": m})),
                Err(OfferError::Upstream(c)) => {
                    json_reply("502 Bad Gateway", json!({"error": format!("The voice service refused the call ({c}).")}))
                }
            }
        }
        "/voice/talk" => {
            let ok = voice.command(PageCmd::Talking { on: v["on"].as_bool().unwrap_or(false), seq: v["seq"].as_u64().unwrap_or(0) });
            json_reply(if ok { "200 OK" } else { "404 Not Found" }, json!({"ok": ok}))
        }
        _ => {
            let ok = voice.command(PageCmd::End);
            json_reply(if ok { "200 OK" } else { "404 Not Found" }, json!({"ok": ok}))
        }
    }
}

/// The Origin header must name this very host.
fn origin_ok(req: &Request) -> bool {
    let (Some(origin), Some(host)) = (req.header("origin"), req.header("host")) else {
        return false;
    };
    origin.split_once("://").is_some_and(|(_, h)| h == host)
}

struct ClientGuard(Arc<Shared>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        *self.0.clients.lock().unwrap() -= 1;
    }
}

async fn handle_ws(shared: Arc<Shared>, mut stream: TcpStream, req: Request) -> anyhow::Result<()> {
    let authed = req.cookie(COOKIE).is_some_and(|t| shared.session_alive(t));
    if !origin_ok(&req) || !authed {
        let mut head = vec![0u8; MAX_HEADERS];
        let _ = stream.read(&mut head).await;
        let status = if authed || req.cookie(COOKIE).is_none() { "403 Forbidden" } else { "401 Unauthorized" };
        stream.write_all(&respond(status, "text/plain", &[], b"denied")).await?;
        return Ok(());
    }
    let token = req.cookie(COOKIE).unwrap_or_default().to_string();
    let admitted = {
        let mut n = shared.clients.lock().unwrap();
        let ok = *n < MAX_CLIENTS;
        if ok {
            *n += 1;
        }
        ok
    };
    if !admitted {
        let mut head = vec![0u8; MAX_HEADERS];
        let _ = stream.read(&mut head).await;
        stream.write_all(&respond("429 Too Many Requests", "text/plain", &[], b"busy")).await?;
        return Ok(());
    }
    let _guard = ClientGuard(shared.clone());

    // Room for one upload chunk per frame; the page and app send 256 KB.
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(1024 * 1024),
        max_frame_size: Some(1024 * 1024),
        ..Default::default()
    };
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(config)).await?;
    let (mut sink, mut source) = ws.split();
    // The open tab, as the viewer names it (with any `<server>/` prefix).
    let mut surface: Option<String> = None;
    let mut last_tree = String::new();
    let mut last_screen = String::new();
    // The open tab's transcript stamp as last sent.
    let mut last_chat: Option<String> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut n: u64 = 0;
    let mut pending: Option<Pending> = None;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                n += 1;
                if !shared.session_alive(&token) {
                    let _ = sink.send(Message::Text(json!({"t":"error","msg":"The session expired."}).to_string())).await;
                    break;
                }
                if n % 4 == 1 {
                    match tree_message(&shared).await {
                        Ok(m) => {
                            if m != last_tree {
                                last_tree = m.clone();
                                if sink.send(Message::Text(m)).await.is_err() { break; }
                            }
                        }
                        Err(e) => {
                            let m = json!({"t":"error","msg":format!("{e}")}).to_string();
                            if sink.send(Message::Text(m)).await.is_err() { break; }
                        }
                    }
                }
                if let Some((server, s)) = surface.as_deref().and_then(|id| shared.route(id)) {
                    let id = surface.clone().unwrap_or_default();
                    if let Ok(text) = server.host.screen(s).await {
                        if text != last_screen {
                            last_screen = text.clone();
                            let m = json!({"t":"screen","surface":id,"text":text}).to_string();
                            if sink.send(Message::Text(m)).await.is_err() { break; }
                        }
                    }
                    // About once a second, a fresh chat snapshot when the transcript changed.
                    if n % 2 == 1 {
                        if let Some((stamp, chat)) = server.host.chat(s, last_chat.as_deref()).await {
                            last_chat = Some(stamp);
                            let m = json!({"t":"chat","surface":id,"agent":chat["agent"],"messages":chat["messages"]}).to_string();
                            if sink.send(Message::Text(m)).await.is_err() { break; }
                        }
                    }
                }
            }
            msg = source.next() => {
                let Some(Ok(msg)) = msg else { break };
                if let Message::Binary(bytes) = &msg {
                    // The bytes of the announced upload, in order.
                    let Some(p) = pending.as_mut() else { continue };
                    let reply = match p.push(bytes) {
                        Step::More => None,
                        Step::TooMany => {
                            let id = p.id.clone();
                            pending = None;
                            Some(json!({"t":"upload_error","id":id,"msg":"More bytes than announced."}))
                        }
                        Step::Done => {
                            let p = pending.take().expect("pending upload");
                            let id = p.id.clone();
                            // The file goes to the server of the open tab, where the agent can read it.
                            let server = surface.as_deref().and_then(|s| shared.route(s)).map(|(v, _)| v)
                                .unwrap_or(&shared.servers[0]);
                            Some(match server.host.save(p).await {
                                Ok(path) => json!({"t":"uploaded","id":id,"path":path}),
                                Err(_) => json!({"t":"upload_error","id":id,"msg":format!("{} could not save the file.", server.name)}),
                            })
                        }
                    };
                    if let Some(r) = reply {
                        if sink.send(Message::Text(r.to_string())).await.is_err() { break; }
                    }
                    continue;
                }
                let Message::Text(t) = msg else {
                    if matches!(msg, Message::Close(_)) { break; }
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                match v["t"].as_str() {
                    Some("open") => {
                        if let Some(s) = v["surface"].as_str().filter(|s| shared.route(s).is_some()) {
                            surface = Some(s.to_string());
                            last_screen.clear();
                            last_chat = None;
                            n = 0;
                        }
                    }
                    Some("close") => surface = None,
                    Some("upload") => {
                        let id = v["id"].as_str().unwrap_or("");
                        let started = if !shared.allow_typing {
                            Err("Typing is off for this share.")
                        } else {
                            Pending::start(id, v["name"].as_str().unwrap_or(""), v["size"].as_u64().unwrap_or(0))
                        };
                        match started {
                            Ok(p) => pending = Some(p),
                            Err(msg) => {
                                pending = None;
                                let m = json!({"t":"upload_error","id":id,"msg":msg}).to_string();
                                if sink.send(Message::Text(m)).await.is_err() { break; }
                            }
                        }
                    }
                    Some(kind @ ("send" | "key")) => {
                        let reply = if !shared.allow_typing {
                            Some("Typing is off for this share.".to_string())
                        } else if let Some((server, s)) = v["surface"].as_str().and_then(|id| shared.route(id)) {
                            // Only the tab the viewer has open.
                            if surface.as_deref() != v["surface"].as_str() {
                                Some("Open the tab first.".to_string())
                            } else if kind == "send" {
                                let text = v["text"].as_str().unwrap_or("");
                                if text.len() > MAX_TEXT {
                                    Some("Text too long.".to_string())
                                } else {
                                    let mut text = text.replace('\n', " ");
                                    if v["enter"].as_bool().unwrap_or(false) { text.push('\n'); }
                                    server.host.send(s, &text).await.err().map(|e| format!("{}: {e}", server.name))
                                }
                            } else {
                                match v["key"].as_str().filter(|k| KEYS.contains(k)) {
                                    Some(k) => server.host.key(s, k).await.err().map(|e| format!("{}: {e}", server.name)),
                                    None => Some("Unknown key.".to_string()),
                                }
                            }
                        } else {
                            Some("Bad tab.".to_string())
                        };
                        if let Some(msg) = reply {
                            let m = json!({"t":"error","msg":msg}).to_string();
                            if sink.send(Message::Text(m)).await.is_err() { break; }
                        } else {
                            // Show the result of the input soon.
                            last_screen.clear();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

pub fn valid_surface(s: &str) -> bool {
    s.strip_prefix("surface:").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The merged tree message, reused by every socket for `TREE_TTL`.
async fn tree_message(shared: &Shared) -> anyhow::Result<String> {
    // Held across the work so concurrent sockets wait for one result.
    let mut cache = shared.tree.lock().await;
    if let Some((at, m)) = &*cache {
        if at.elapsed() < TREE_TTL {
            return Ok(m.clone());
        }
    }
    let trees = futures_util::future::join_all(shared.servers.iter().map(|s| s.host.tree())).await;
    let parts: Vec<(&Server, Result<Value, String>)> =
        shared.servers.iter().zip(trees).map(|(s, t)| (s, t.map_err(|e| format!("{e:#}")))).collect();
    // Without other servers, a dead local daemon is the share's error.
    if !shared.gateway {
        if let Some((_, Err(e))) = parts.first() {
            bail!("{e}");
        }
    }
    let mut m = merge_trees(&parts, shared.allow_typing, shared.gateway);
    if shared.voice.is_some() {
        m["voice"] = json!(true);
    }
    let m = m.to_string();
    *cache = Some((Instant::now(), m.clone()));
    Ok(m)
}

/// One tree message from every server's slim tree. Each workspace names its
/// server; a remote server's ids get its `<name>/` prefix. In gateway mode
/// the message also lists the servers with their connection state.
fn merge_trees(parts: &[(&Server, Result<Value, String>)], typing: bool, gateway: bool) -> Value {
    let mut workspaces = Vec::new();
    let mut servers = Vec::new();
    let mut host = Value::Null;
    for (server, tree) in parts {
        let (mut status, mut error) = server.status();
        if let (Err(e), "connected") = (tree, status) {
            status = "unreachable";
            error = Some(e.clone());
        }
        servers.push(json!({"name": server.name, "status": status, "error": error, "local": server.is_local()}));
        let Ok(tree) = tree else { continue };
        if server.is_local() {
            host = tree["host"].clone();
        }
        let prefix = if server.is_local() { String::new() } else { format!("{}/", server.name) };
        for w in tree["workspaces"].as_array().into_iter().flatten() {
            let mut w = w.clone();
            if !prefix.is_empty() {
                prefix_id(&mut w["id"], &prefix);
                for t in w["tabs"].as_array_mut().into_iter().flatten() {
                    prefix_id(&mut t["id"], &prefix);
                }
            }
            w["server"] = json!(server.name);
            workspaces.push(w);
        }
    }
    let mut m = json!({"t": "tree", "typing": typing, "host": host, "workspaces": workspaces});
    if gateway {
        m["servers"] = json!(servers);
    }
    m
}

fn prefix_id(id: &mut Value, prefix: &str) {
    if let Some(s) = id.as_str() {
        *id = json!(format!("{prefix}{s}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared::new("/nonexistent".into(), false)
    }

    #[test]
    fn a_code_works_once() {
        let s = shared();
        let c = s.new_code();
        assert!(s.redeem("wrong").is_none());
        let tok = s.redeem(&c).expect("first use");
        assert!(s.session_alive(&tok));
        assert!(s.redeem(&c).is_none(), "second use must fail");
    }

    #[test]
    fn an_expired_code_fails() {
        let s = shared();
        s.codes.lock().unwrap().insert("old".into(), Instant::now() - Duration::from_secs(1));
        assert!(s.redeem("old").is_none());
    }

    #[test]
    fn use_keeps_a_session_alive_until_its_end() {
        let s = shared();
        let now = Instant::now();
        let sess = |idle, end| Session { idle, end };
        let soon = now + Duration::from_secs(5);
        s.sessions.lock().unwrap().insert("used".into(), sess(soon, now + SESSION_MAX));
        s.sessions.lock().unwrap().insert("capped".into(), sess(soon, now + Duration::from_secs(60)));
        s.sessions.lock().unwrap().insert("idle".into(), sess(now - Duration::from_secs(1), now + SESSION_MAX));
        s.sessions.lock().unwrap().insert("over".into(), sess(soon, now - Duration::from_secs(1)));

        assert!(s.session_alive("used"));
        assert!(s.sessions.lock().unwrap()["used"].idle > now + SESSION_IDLE - Duration::from_secs(5));
        assert!(s.session_alive("capped"));
        assert!(s.sessions.lock().unwrap()["capped"].idle <= now + Duration::from_secs(60));
        assert!(!s.session_alive("idle"));
        assert!(!s.session_alive("over"));
    }

    #[test]
    fn origin_must_match_host() {
        let mk = |o: Option<&str>| {
            let mut headers = HashMap::new();
            headers.insert("host".to_string(), "a.trycloudflare.com".to_string());
            if let Some(o) = o {
                headers.insert("origin".to_string(), o.to_string());
            }
            Request { method: "GET".into(), path: "/ws".into(), headers }
        };
        assert!(origin_ok(&mk(Some("https://a.trycloudflare.com"))));
        assert!(!origin_ok(&mk(Some("https://evil.example"))));
        assert!(!origin_ok(&mk(None)));
    }

    #[test]
    fn cookie_is_found_among_others() {
        let r = parse_head("GET /ws HTTP/1.1\r\nCookie: a=1; lmux_session=tok; b=2\r\nHost: h").unwrap();
        assert_eq!(r.cookie(COOKIE), Some("tok"));
    }

    #[test]
    fn surfaces_are_validated() {
        assert!(valid_surface("surface:12"));
        assert!(!valid_surface("surface:"));
        assert!(!valid_surface("surface:1;rm"));
        assert!(!valid_surface("workspace:1"));
    }

    #[test]
    fn tree_is_flattened() {
        use super::super::agents::TabAgent;
        use super::super::host::tree_to_message;
        let tree = json!({"host":"h","workspaces":[{"id":"workspace:1","title":"w","agent":"idle","summary":null,
            "panes":[{"surfaces":[{"id":"surface:1","title":"a","kind":"terminal","cols":80}]},
                     {"surfaces":[{"id":"surface:2","title":"b","kind":"browser"}]}]}]});
        let mut agents = HashMap::new();
        agents.insert(
            "surface:1".to_string(),
            TabAgent { agent: Some("claude"), command: Some("2.1.296".into()), transcript: Some("/t.jsonl".into()) },
        );
        let m = tree_to_message(&tree, &agents);
        assert_eq!(m["workspaces"][0]["tabs"].as_array().unwrap().len(), 2);
        assert!(m["workspaces"][0]["tabs"][0].get("cols").is_none());
        let a = &m["workspaces"][0]["tabs"][0];
        assert_eq!((&a["agent"], &a["command"], &a["chat"]), (&json!("claude"), &json!("2.1.296"), &json!(true)));
        // Browser tabs and tabs the daemon did not describe carry no agent.
        let b = &m["workspaces"][0]["tabs"][1];
        assert_eq!((&b["agent"], &b["chat"]), (&Value::Null, &json!(false)));
        // The transcript path itself never reaches the page.
        assert!(!m.to_string().contains("t.jsonl"));
    }

    fn slim(host: &str) -> Value {
        json!({"host": host, "workspaces": [{"id": "workspace:1", "title": "w",
            "tabs": [{"id": "surface:4", "title": "t", "kind": "terminal"}]}]})
    }

    #[tokio::test]
    async fn trees_merge_with_server_prefixes() {
        let s = Shared::with_servers("/nonexistent".into(), vec!["vps1".into(), "vps2".into()], true);
        let local_name = s.servers[0].name.clone();
        let parts = vec![
            (&s.servers[0], Ok(slim("mac"))),
            (&s.servers[1], Ok(slim("vps1"))),
            (&s.servers[2], Err("ssh ended".to_string())),
        ];
        let m = merge_trees(&parts, true, true);
        assert_eq!(m["typing"], true);
        assert_eq!(m["host"], "mac");
        let ws = m["workspaces"].as_array().unwrap();
        assert_eq!(ws.len(), 2);
        assert_eq!((&ws[0]["id"], &ws[0]["server"]), (&json!("workspace:1"), &json!(local_name)));
        assert_eq!(ws[0]["tabs"][0]["id"], "surface:4");
        assert_eq!((&ws[1]["id"], &ws[1]["server"]), (&json!("vps1/workspace:1"), &json!("vps1")));
        assert_eq!(ws[1]["tabs"][0]["id"], "vps1/surface:4");
        let servers = m["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 3);
        assert_eq!((&servers[0]["status"], &servers[0]["local"]), (&json!("connected"), &json!(true)));
        assert_eq!(servers[2]["name"], "vps2");
        // Still on its first try: no error to show yet.
        assert_eq!(servers[2]["status"], "connecting");
    }

    #[test]
    fn a_plain_share_lists_no_servers() {
        let s = shared();
        let m = merge_trees(&[(&s.servers[0], Ok(slim("mac")))], false, false);
        assert!(m.get("servers").is_none());
        assert!(m["workspaces"][0].get("server").is_some());
    }

    #[tokio::test]
    async fn actions_route_to_the_owning_server() {
        let s = Shared::with_servers("/nonexistent".into(), vec!["vps1".into()], true);
        let (v, id) = s.route("surface:3").unwrap();
        assert!(v.is_local());
        assert_eq!(id, "surface:3");
        let (v, id) = s.route("vps1/surface:3").unwrap();
        assert_eq!((v.name.as_str(), id), ("vps1", "surface:3"));
        assert!(s.route("vps9/surface:3").is_none(), "unknown server");
        assert!(s.route("vps1/surface:x").is_none(), "bad tab");
        // The local server is never named by prefix.
        let local = s.servers[0].name.clone();
        assert!(s.route(&format!("{local}/surface:3")).is_none());
    }
}
