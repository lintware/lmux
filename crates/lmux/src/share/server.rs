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
use super::voice::{CmdResult, OfferError, PageCmd, Voice, VoiceConfig, MAX_SDP};
use super::voice_tools::Backend;
use super::auth::{Auth, Who, GRACE};

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

/// Where this share can be reached. `urls` is in the order clients should
/// try (tailnet first, the public tunnel last); `primary` is the public one
/// that pairing links use. `version` changes with every change, so each
/// socket knows when to tell its client.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Endpoints {
    pub primary: Option<String>,
    pub urls: Vec<String>,
    pub version: u64,
}

/// How a client takes a successor credential, from its `hello`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Rollover {
    /// The app: the token itself, which it stores and acknowledges.
    Token,
    /// A web page: a code it trades at `/auth`, so the cookie stays HttpOnly.
    Code,
}

pub struct Shared {
    local: Arc<Local>,
    servers: Vec<Arc<Server>>,
    attached: Mutex<HashMap<String, Arc<Server>>>,
    /// True when other servers are controlled too (gateway mode).
    gateway: bool,
    allow_typing: bool,
    pub auth: Auth,
    endpoints: Mutex<Endpoints>,
    clients: Mutex<usize>,
    /// The last merged tree message, shared by every socket for `TREE_TTL`.
    tree: tokio::sync::Mutex<Option<(Instant, String)>>,
    /// The voice call, when a key is set up (and `LMUX_VOICE` is not `0`).
    voice: Option<Arc<Voice>>,
}

impl Shared {
    #[cfg(test)]
    pub fn new(socket: String, allow_typing: bool) -> Self {
        Self::with_servers(socket, Vec::new(), allow_typing, Auth::in_memory())
    }

    /// This machine plus the servers reached over SSH at `targets`.
    pub fn with_servers(socket: String, targets: Vec<String>, allow_typing: bool, auth: Auth) -> Self {
        let local = Arc::new(Local::new(socket));
        let mut servers = vec![Arc::new(Server { name: lmux_core::daemon::hostname(), host: Host::Local(local.clone()) })];
        let gateway = !targets.is_empty();
        for t in targets {
            servers.push(Arc::new(Server { name: t.clone(), host: Host::Remote(Remote::start(t)) }));
        }
        Shared {
            local,
            servers,
            attached: Mutex::new(HashMap::new()),
            gateway,
            allow_typing,
            auth,
            endpoints: Mutex::new(Endpoints::default()),
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
        self.auth.new_code()
    }

    /// Whether `code` is still waiting to be scanned.
    pub fn code_live(&self, code: &str) -> bool {
        self.auth.code_live(code)
    }

    /// Record where the share is reachable now; connected clients hear of a
    /// change on their next tick.
    pub fn set_endpoints(&self, primary: Option<String>, urls: Vec<String>) {
        let mut e = self.endpoints.lock().unwrap();
        if e.primary != primary || e.urls != urls {
            *e = Endpoints { primary, urls, version: e.version + 1 };
        }
    }

    pub fn endpoints(&self) -> Endpoints {
        self.endpoints.lock().unwrap().clone()
    }

    pub fn allow_typing(&self) -> bool {
        self.allow_typing
    }

    /// The names of the servers this share controls, this machine first.
    pub fn server_names(&self) -> Vec<String> {
        self.all_servers().iter().map(|s| s.name.clone()).collect()
    }

    fn all_servers(&self) -> Vec<Arc<Server>> {
        let mut servers = self.servers.clone();
        let attached = self.attached.lock().unwrap();
        let mut names: Vec<_> = attached.keys().collect();
        names.sort();
        servers.extend(names.into_iter().map(|n| attached[n].clone()));
        servers
    }

    pub async fn attach(&self, name: String, stream: tokio::net::UnixStream) -> anyhow::Result<()> {
        anyhow::ensure!(!name.is_empty() && name.len() <= 255 && !name.contains('/') && !name.chars().any(char::is_control) && !self.servers.iter().any(|s| s.name == name), "invalid or duplicate server name");
        let remote = Remote::attached(name.clone());
        self.attached.lock().unwrap().insert(name.clone(), Arc::new(Server { name, host: Host::Remote(remote.clone()) }));
        *self.tree.lock().await = None;
        remote.serve_attached(stream).await
    }

    /// The server that owns tab `id`, and the tab's id on that server.
    fn route<'a>(&self, id: &'a str) -> Option<(Arc<Server>, &'a str)> {
        let (server, surface) = match id.split_once('/') {
            Some((name, s)) => (self.all_servers().into_iter().find(|v| !v.is_local() && v.name == name)?, s),
            None => (self.servers.first()?.clone(), id),
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
        let servers = self.0.all_servers();
        let all = futures_util::future::join_all(servers.iter().map(|s| s.host.notifications())).await;
        // A server that cannot answer (an old lmux, say) just has none.
        Ok(servers.iter().zip(all).flat_map(|(s, r)| r.unwrap_or_default().into_iter().map(|n| (s.name.clone(), n))).collect())
    }
}

pub fn random_token() -> String {
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .expect("the system random source is readable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Accept clients on `listener`. `secure` says whether its clients reach it
/// over HTTPS (the tunnel), which decides the cookie's `Secure` flag: the
/// tailnet listener is plain HTTP inside WireGuard, where a `Secure` cookie
/// would never be stored.
pub async fn serve(shared: Arc<Shared>, listener: TcpListener, secure: bool) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let shared = shared.clone();
        tokio::spawn(async move {
            // Errors are one client's problem; frames and bodies are never logged.
            let _ = tokio::time::timeout(Duration::from_secs(3 * 3600), handle(shared, stream, secure)).await;
        });
    }
}

fn session_cookie(token: &str, max_age: u64, secure: bool) -> String {
    let secure = if secure { " Secure;" } else { "" };
    format!("{COOKIE}={token}; HttpOnly;{secure} SameSite=Strict; Path=/; Max-Age={max_age}")
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

async fn handle(shared: Arc<Shared>, mut stream: TcpStream, secure: bool) -> anyhow::Result<()> {
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
                // A pairing, handoff or (web) rollover code: each sets the cookie.
                match shared.auth.redeem(&code) {
                    Some(g) => {
                        let cookie = session_cookie(&g.token, g.max_age, secure);
                        respond("204 No Content", "text/plain", &[("Set-Cookie", cookie)], b"")
                    }
                    None => respond("401 Unauthorized", "text/plain", &[], b"expired"),
                }
            }
        }
        // The client stored its successor credential and sends it back: from
        // now its predecessor only lingers briefly.
        ("POST", "/rollover/ack") => {
            if !origin_ok(&req) {
                respond("403 Forbidden", "text/plain", &[], b"bad origin")
            } else if req.cookie(COOKIE).and_then(|t| shared.auth.check(t)).is_some() {
                respond("204 No Content", "text/plain", &[], b"")
            } else {
                respond("401 Unauthorized", "text/plain", &[], b"expired")
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
    // The call belongs to the lineage, so a rollover mid-call keeps it.
    let Some(token) = req.cookie(COOKIE).and_then(|t| shared.auth.check(t)).map(|w| w.lineage) else {
        return json_reply("401 Unauthorized", json!({"error": "Sign in again."}));
    };
    let token = token.as_str();
    let Some(voice) = &shared.voice else {
        return json_reply("404 Not Found", json!({"error": "Voice is not set up on this share."}));
    };
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match path {
        "/voice/offer" => {
            let Some(sdp) = v["sdp"].as_str() else {
                return json_reply("400 Bad Request", json!({"error": "An SDP offer is required."}));
            };
            match voice.offer(Arc::new(ShareBackend(shared.clone())), sdp, token).await {
                Ok(answer) => json_reply("201 Created", json!({"sdp": answer})),
                Err(OfferError::Busy(m)) => json_reply("429 Too Many Requests", json!({"error": m})),
                Err(OfferError::Bad(m)) => json_reply("400 Bad Request", json!({"error": m})),
                Err(OfferError::Upstream(c)) => {
                    json_reply("502 Bad Gateway", json!({"error": format!("The voice service refused the call ({c}).")}))
                }
            }
        }
        _ => {
            let cmd = if path == "/voice/talk" {
                PageCmd::Talking { on: v["on"].as_bool().unwrap_or(false), seq: v["seq"].as_u64().unwrap_or(0) }
            } else {
                PageCmd::End
            };
            voice_cmd_reply(voice.command(token, cmd))
        }
    }
}

fn voice_cmd_reply(r: CmdResult) -> Vec<u8> {
    match r {
        CmdResult::Sent => json_reply("200 OK", json!({"ok": true})),
        CmdResult::NoCall => json_reply("404 Not Found", json!({"ok": false})),
        CmdResult::NotOwner => json_reply(
            "403 Forbidden",
            json!({"ok": false, "error": "The call was started by another phone."}),
        ),
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
    let who = req.cookie(COOKIE).and_then(|t| shared.auth.check(t));
    let authed = who.is_some();
    if !origin_ok(&req) || !authed {
        let mut head = vec![0u8; MAX_HEADERS];
        let _ = stream.read(&mut head).await;
        let status = if authed || req.cookie(COOKIE).is_none() { "403 Forbidden" } else { "401 Unauthorized" };
        stream.write_all(&respond(status, "text/plain", &[], b"denied")).await?;
        return Ok(());
    }
    let Some(mut who) = who else { return Ok(()) };
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
    // Set by the client's `hello`; older clients never roll over.
    let mut rollover: Option<Rollover> = None;
    let mut told_endpoints = 0u64;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                n += 1;
                // The socket's credential, or the successor it rolled over to.
                // None: revoked, or lapsed.
                match shared.auth.follow(&who) {
                    Some(w) => who = w,
                    None => {
                        let _ = sink.send(Message::Text(json!({"t":"error","msg":"The session expired."}).to_string())).await;
                        break;
                    }
                }
                if let Some(m) = endpoints_message(&shared, &mut told_endpoints) {
                    if sink.send(Message::Text(m)).await.is_err() { break; }
                }
                // About once every 10 s, well under the resend window.
                if n % 20 == 2 {
                    if let Some(m) = rollover_message(&shared, &who, rollover) {
                        if sink.send(Message::Text(m)).await.is_err() { break; }
                    }
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
                                .unwrap_or_else(|| shared.servers[0].clone());
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
                    Some("hello") => {
                        rollover = match v["rollover"].as_str() {
                            Some("token") => Some(Rollover::Token),
                            Some("code") => Some(Rollover::Code),
                            _ => None,
                        };
                        if let Some(m) = endpoints_message(&shared, &mut told_endpoints) {
                            if sink.send(Message::Text(m)).await.is_err() { break; }
                        }
                        if let Some(m) = rollover_message(&shared, &who, rollover) {
                            if sink.send(Message::Text(m)).await.is_err() { break; }
                        }
                    }
                    // A page asks for a way in on the share's other addresses.
                    Some("handoff") => {
                        let m = json!({"t":"handoff","code":shared.auth.handoff(&who),"expires_in":GRACE}).to_string();
                        if sink.send(Message::Text(m)).await.is_err() { break; }
                    }
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

/// The share's addresses, when they changed since this socket last heard.
fn endpoints_message(shared: &Shared, told: &mut u64) -> Option<String> {
    let e = shared.endpoints();
    if e.version == 0 || e.version == *told {
        return None;
    }
    *told = e.version;
    Some(json!({"t":"endpoints","primary":e.primary,"urls":e.urls}).to_string())
}

/// A successor credential for this socket's client, when one is due.
fn rollover_message(shared: &Shared, who: &Who, how: Option<Rollover>) -> Option<String> {
    let how = how?;
    if !shared.auth.rollover_due(who) {
        return None;
    }
    Some(match how {
        Rollover::Token => {
            let g = shared.auth.offer_token(who)?;
            json!({"t":"rollover","token":g.token,"expires_in":g.max_age}).to_string()
        }
        Rollover::Code => json!({"t":"rollover","code":shared.auth.offer_code(who)}).to_string(),
    })
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
    let servers = shared.all_servers();
    let trees = futures_util::future::join_all(servers.iter().map(|s| s.host.tree())).await;
    let parts: Vec<(&Server, Result<Value, String>)> =
        servers.iter().zip(trees).map(|(s, t)| (s.as_ref(), t.map_err(|e| format!("{e:#}")))).collect();
    // Without other servers, a dead local daemon is the share's error.
    let gateway = shared.gateway || servers.len() > 1;
    if !gateway {
        if let Some((_, Err(e))) = parts.first() {
            bail!("{e}");
        }
    }
    let mut m = merge_trees(&parts, shared.allow_typing, gateway);
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
    fn another_session_cannot_drive_the_call() {
        let r = voice_cmd_reply(CmdResult::NotOwner);
        let r = String::from_utf8_lossy(&r);
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        assert!(r.contains("another phone"));
        assert!(String::from_utf8_lossy(&voice_cmd_reply(CmdResult::Sent)).starts_with("HTTP/1.1 200"));
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
        let s = Shared::with_servers("/nonexistent".into(), vec!["vps1".into(), "vps2".into()], true, Auth::in_memory());
        let local_name = s.servers[0].name.clone();
        let parts: Vec<(&Server, Result<Value, String>)> = vec![
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
        let s = Shared::with_servers("/nonexistent".into(), vec!["vps1".into()], true, Auth::in_memory());
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

    #[tokio::test]
    async fn laptop_attachment_routes_and_reconnects_without_inbound_ssh() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let shared = Arc::new(Shared::new("/nonexistent".into(), true));
        for generation in 1..=2 {
            let (gateway, laptop) = tokio::net::UnixStream::pair().unwrap();
            let s = shared.clone();
            let attachment = tokio::spawn(async move { s.attach("macbook".into(), gateway).await });
            let (r, mut w) = laptop.into_split();
            w.write_all(b"{\"hello\":1}\n").await.unwrap();
            let backend = tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                while let Some(line) = lines.next_line().await.unwrap() {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let result = match req["op"].as_str().unwrap() {
                        "tree" => slim("macbook"),
                        "screen" => json!({"text": format!("laptop {generation}")}),
                        "send" => { assert_eq!(req["text"], "hello laptop"); Value::Null },
                        "key" => { assert_eq!(req["key"], "enter"); Value::Null },
                        op => panic!("unexpected op: {op}"),
                    };
                    if req["op"] != "tree" { assert_eq!(req["surface"], "surface:4"); }
                    w.write_all(format!("{}\n", json!({"id":req["id"], "ok":result})).as_bytes()).await.unwrap();
                }
            });
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if shared.route("macbook/surface:4").is_some_and(|(s, _)| s.status().0 == "connected") { break; }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            let tree: Value = serde_json::from_str(&tree_message(&shared).await.unwrap()).unwrap();
            assert_eq!(tree["workspaces"][0]["tabs"][0]["id"], "macbook/surface:4");
            assert_eq!(tree["workspaces"][0]["server"], "macbook");
            assert_eq!(shared.server_names().iter().filter(|s| *s == "macbook").count(), 1);
            let (server, surface) = shared.route("macbook/surface:4").unwrap();
            assert_eq!(server.host.screen(surface).await.unwrap(), format!("laptop {generation}"));
            server.host.send(surface, "hello laptop").await.unwrap();
            server.host.key(surface, "enter").await.unwrap();
            assert!(shared.route("surface:4").unwrap().0.is_local(), "bare IDs still belong to the gateway");
            backend.abort();
            let _ = backend.await;
            attachment.await.unwrap().unwrap();
            assert_eq!(server.status().0, "unreachable");
            assert!(server.host.screen(surface).await.is_err());
        }
    }

    #[tokio::test]
    async fn attachments_cannot_shadow_existing_servers_or_escape_id_namespace() {
        let shared = Shared::new("/nonexistent".into(), true);
        for name in ["".to_owned(), "bad/name".to_owned(), "bad\nname".to_owned(), shared.servers[0].name.clone()] {
            let (gateway, _laptop) = tokio::net::UnixStream::pair().unwrap();
            assert!(shared.attach(name, gateway).await.is_err());
        }
        assert_eq!(shared.server_names().len(), 1);
    }
}
