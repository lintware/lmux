//! The share server: a few static files, the code-for-cookie exchange, and
//! one WebSocket that carries the session list, screens and keystrokes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio_tungstenite::tungstenite::Message;

use super::agents::{self, Finder, TabAgent};
use super::{CODE_TTL, SESSION_TTL};

const PAGE: &str = include_str!("page.html");
const JS: &str = include_str!("app.js");
const CSS: &str = include_str!("app.css");

const COOKIE: &str = "lmux_session";
const MAX_HEADERS: usize = 16 * 1024;
const MAX_CLIENTS: usize = 8;
const MAX_TEXT: usize = 4096;
const KEYS: &[&str] = &[
    "enter", "escape", "esc", "tab", "up", "down", "left", "right", "backspace", "ctrl+c", "ctrl+d",
];

/// How long one tree (and the `ps`/`lsof` agent detection behind it) serves
/// every socket's poll, so viewers do not multiply the process scans.
const TREE_TTL: Duration = Duration::from_millis(1500);

/// What is known about each terminal tab, by surface id.
type Tabs = Arc<HashMap<String, TabAgent>>;

/// The last tree message and agent detection, shared by all sockets.
struct TreeCache {
    finder: Finder,
    last: Option<(Instant, String, Tabs)>,
}

pub struct Shared {
    socket: String,
    allow_typing: bool,
    codes: Mutex<HashMap<String, Instant>>,
    sessions: Mutex<HashMap<String, Instant>>,
    clients: Mutex<usize>,
    tree: tokio::sync::Mutex<TreeCache>,
}

impl Shared {
    pub fn new(socket: String, allow_typing: bool) -> Self {
        // Agent transcripts live under `~/.claude`, `~/.codex` and `~/.grok`.
        let home = std::env::var_os("HOME").map(Into::into).unwrap_or_default();
        Shared {
            socket,
            allow_typing,
            codes: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            clients: Mutex::new(0),
            tree: tokio::sync::Mutex::new(TreeCache { finder: Finder::new(home), last: None }),
        }
    }

    pub fn new_code(&self) -> String {
        let code = random_token();
        self.codes.lock().unwrap().insert(code.clone(), Instant::now() + CODE_TTL);
        code
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
        self.sessions.lock().unwrap().insert(token.clone(), now + SESSION_TTL);
        Some(token)
    }

    fn session_alive(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        s.retain(|_, exp| *exp > now);
        s.keys().any(|k| ct_eq(k.as_bytes(), token.as_bytes()))
    }
}

fn random_token() -> String {
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
        if len > 1024 {
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
                            SESSION_TTL.as_secs()
                        );
                        respond("204 No Content", "text/plain", &[("Set-Cookie", cookie)], b"")
                    }
                    None => respond("401 Unauthorized", "text/plain", &[], b"expired"),
                }
            }
        }
        _ => respond("404 Not Found", "text/plain", &[], b"not found"),
    };
    stream.write_all(&out).await?;
    stream.shutdown().await.ok();
    Ok(())
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

    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();
    let mut surface: Option<String> = None;
    let mut last_tree = String::new();
    let mut last_screen = String::new();
    // What is known about each tab, and the open tab's transcript
    // (path, mtime, length) as last sent.
    let mut tabs: Tabs = Arc::default();
    let mut last_chat: Option<(std::path::PathBuf, std::time::SystemTime, u64)> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut n: u64 = 0;

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
                        Ok((m, t)) => {
                            tabs = t;
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
                if let Some(s) = &surface {
                    if let Ok(r) = rpc(&shared.socket, "term.read_screen", json!({"surface": s})).await {
                        let text = r["text"].as_str().unwrap_or("").to_string();
                        if text != last_screen {
                            last_screen = text.clone();
                            let m = json!({"t":"screen","surface":s,"text":text}).to_string();
                            if sink.send(Message::Text(m)).await.is_err() { break; }
                        }
                    }
                    // About once a second, a fresh chat snapshot when the transcript changed.
                    if n % 2 == 1 {
                        if let Some(m) = chat_message(s, tabs.get(s), &mut last_chat).await {
                            if sink.send(Message::Text(m)).await.is_err() { break; }
                        }
                    }
                }
            }
            msg = source.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(t) = msg else {
                    if matches!(msg, Message::Close(_)) { break; }
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                match v["t"].as_str() {
                    Some("open") => {
                        if let Some(s) = v["surface"].as_str().filter(|s| valid_surface(s)) {
                            surface = Some(s.to_string());
                            last_screen.clear();
                            last_chat = None;
                            n = 0;
                        }
                    }
                    Some("close") => surface = None,
                    Some(kind @ ("send" | "key")) => {
                        let reply = if !shared.allow_typing {
                            Some("Typing is off for this share.")
                        } else if let Some(s) = v["surface"].as_str().filter(|s| valid_surface(s)) {
                            // Only the tab the viewer has open.
                            if surface.as_deref() != Some(s) {
                                Some("Open the tab first.")
                            } else if kind == "send" {
                                let text = v["text"].as_str().unwrap_or("");
                                if text.len() > MAX_TEXT {
                                    Some("Text too long.")
                                } else {
                                    let mut text = text.replace('\n', " ");
                                    if v["enter"].as_bool().unwrap_or(false) { text.push('\n'); }
                                    let _ = rpc(&shared.socket, "term.send", json!({"surface": s, "text": text})).await;
                                    None
                                }
                            } else {
                                match v["key"].as_str().filter(|k| KEYS.contains(k)) {
                                    Some(k) => { let _ = rpc(&shared.socket, "term.send_key", json!({"surface": s, "key": k})).await; None }
                                    None => Some("Unknown key."),
                                }
                            }
                        } else {
                            Some("Bad tab.")
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

fn valid_surface(s: &str) -> bool {
    s.strip_prefix("surface:").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The tree message and agent detection, reused by every socket for `TREE_TTL`.
async fn tree_message(shared: &Shared) -> anyhow::Result<(String, Tabs)> {
    // Held across the work so concurrent sockets wait for one result.
    let mut cache = shared.tree.lock().await;
    if let Some((at, m, tabs)) = &cache.last {
        if at.elapsed() < TREE_TTL {
            return Ok((m.clone(), tabs.clone()));
        }
    }
    let tree = rpc(&shared.socket, "system.tree", json!({})).await?;
    // An older daemon has no `term.processes`; tabs then carry no agent info.
    let procs = rpc(&shared.socket, "term.processes", json!({})).await.unwrap_or(Value::Null);
    let tabs = Arc::new(cache.finder.detect(&procs).await);
    let m = tree_to_message(&tree, shared.allow_typing, &tabs).to_string();
    cache.last = Some((Instant::now(), m.clone(), tabs.clone()));
    Ok((m, tabs))
}

/// A `chat` frame for the open tab when its transcript changed since `last`.
async fn chat_message(
    surface: &str,
    tab: Option<&TabAgent>,
    last: &mut Option<(std::path::PathBuf, std::time::SystemTime, u64)>,
) -> Option<String> {
    let (agent, path) = tab.and_then(|t| Some((t.agent?, t.transcript.clone()?)))?;
    let meta = tokio::fs::metadata(&path).await.ok()?;
    let stamp = (path.clone(), meta.modified().ok()?, meta.len());
    if last.as_ref() == Some(&stamp) {
        return None;
    }
    *last = Some(stamp);
    let messages = tokio::task::spawn_blocking(move || agents::read_chat(agent, &path)).await.ok()?.ok()?;
    Some(json!({"t":"chat","surface":surface,"agent":agent,"messages":messages}).to_string())
}

/// The slim tree the page needs: workspaces with their tabs flattened, and
/// what is known about the agent in each terminal tab.
pub fn tree_to_message(tree: &Value, typing: bool, agents: &HashMap<String, TabAgent>) -> Value {
    let workspaces: Vec<Value> = tree["workspaces"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|w| {
            let tabs: Vec<Value> = w["panes"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .flat_map(|p| p["surfaces"].as_array().cloned().unwrap_or_default())
                .map(|s| {
                    let a = s["id"].as_str().and_then(|id| agents.get(id));
                    json!({
                        "id": s["id"], "title": s["title"], "kind": s["kind"],
                        "agent": a.and_then(|a| a.agent),
                        "command": a.and_then(|a| a.command.clone()),
                        "chat": a.is_some_and(|a| a.transcript.is_some()),
                    })
                })
                .collect();
            json!({
                "id": w["id"], "title": w["title"], "agent": w["agent"],
                "summary": w["summary"], "cwd": w["cwd"], "git_branch": w["git_branch"], "tabs": tabs,
            })
        })
        .collect();
    json!({"t": "tree", "typing": typing, "host": tree["host"], "workspaces": workspaces})
}

/// One request to the local daemon over its unix socket.
async fn rpc(socket: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    let stream = UnixStream::connect(socket).await.context("connecting to the lmux daemon")?;
    let (r, mut w) = stream.into_split();
    let mut line = json!({"id": 1, "method": method, "params": params}).to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut resp = String::new();
    tokio::time::timeout(Duration::from_secs(6), BufReader::new(r).read_line(&mut resp))
        .await
        .context("the daemon did not answer")??;
    let v: Value = serde_json::from_str(resp.trim())?;
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        bail!("{}", e["message"].as_str().unwrap_or("daemon error"));
    }
    Ok(v["result"].clone())
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
        let tree = json!({"host":"h","workspaces":[{"id":"workspace:1","title":"w","agent":"idle","summary":null,
            "panes":[{"surfaces":[{"id":"surface:1","title":"a","kind":"terminal","cols":80}]},
                     {"surfaces":[{"id":"surface:2","title":"b","kind":"browser"}]}]}]});
        let mut agents = HashMap::new();
        agents.insert(
            "surface:1".to_string(),
            TabAgent { agent: Some("claude"), command: Some("2.1.296".into()), transcript: Some("/t.jsonl".into()) },
        );
        let m = tree_to_message(&tree, true, &agents);
        assert_eq!(m["workspaces"][0]["tabs"].as_array().unwrap().len(), 2);
        assert_eq!(m["typing"], true);
        assert!(m["workspaces"][0]["tabs"][0].get("cols").is_none());
        let a = &m["workspaces"][0]["tabs"][0];
        assert_eq!((&a["agent"], &a["command"], &a["chat"]), (&json!("claude"), &json!("2.1.296"), &json!(true)));
        // Browser tabs and tabs the daemon did not describe carry no agent.
        let b = &m["workspaces"][0]["tabs"][1];
        assert_eq!((&b["agent"], &b["chat"]), (&Value::Null, &json!(false)));
        // The transcript path itself never reaches the page.
        assert!(!m.to_string().contains("t.jsonl"));
    }
}
