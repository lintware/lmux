//! One machine's sessions, as the share sees them: the slim tree with agent
//! info, screens, transcripts as chat, input, and uploads.
//!
//! [`Local`] does the work against this machine's daemon. A gateway reaches
//! other servers through [`super::remote::Remote`], which runs `lmux
//! share-backend` there over SSH; that backend is a [`Local`] on the far side,
//! so agent detection and transcripts always run where the session lives.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::agents::{self, Finder, TabAgent};
use super::remote::Remote;
use super::upload::{self, Pending};

/// How long one tree (and the `ps`/`lsof` agent detection behind it) serves
/// every poll, so viewers do not multiply the process scans.
pub const TREE_TTL: Duration = Duration::from_millis(1500);

type Tabs = Arc<std::collections::HashMap<String, TabAgent>>;

struct TreeCache {
    finder: Finder,
    last: Option<(Instant, Value, Tabs)>,
}

/// This machine's daemon.
pub struct Local {
    socket: String,
    tree: tokio::sync::Mutex<TreeCache>,
    /// Where files sent from the phone go (made on first upload).
    uploads: std::path::PathBuf,
}

impl Local {
    pub fn new(socket: String) -> Self {
        // Agent transcripts live under `~/.claude`, `~/.codex` and `~/.grok`.
        let home = std::env::var_os("HOME").map(Into::into).unwrap_or_default();
        Local {
            socket,
            tree: tokio::sync::Mutex::new(TreeCache { finder: Finder::new(home), last: None }),
            uploads: upload::folder(&super::server::random_token()),
        }
    }

    pub fn uploads_dir(&self) -> &std::path::Path {
        &self.uploads
    }

    /// The slim tree (see [`tree_to_message`]), reused for [`TREE_TTL`].
    pub async fn tree(&self) -> anyhow::Result<Value> {
        Ok(self.tree_and_tabs().await?.0)
    }

    async fn tree_and_tabs(&self) -> anyhow::Result<(Value, Tabs)> {
        // Held across the work so concurrent callers wait for one result.
        let mut cache = self.tree.lock().await;
        if let Some((at, m, tabs)) = &cache.last {
            if at.elapsed() < TREE_TTL {
                return Ok((m.clone(), tabs.clone()));
            }
        }
        let tree = rpc(&self.socket, "system.tree", json!({})).await?;
        // An older daemon has no `term.processes`; tabs then carry no agent info.
        let procs = rpc(&self.socket, "term.processes", json!({})).await.unwrap_or(Value::Null);
        let tabs = Arc::new(cache.finder.detect(&procs).await);
        let m = tree_to_message(&tree, &tabs);
        cache.last = Some((Instant::now(), m.clone(), tabs.clone()));
        Ok((m, tabs))
    }

    pub async fn screen(&self, surface: &str) -> anyhow::Result<String> {
        let r = rpc(&self.socket, "term.read_screen", json!({"surface": surface})).await?;
        Ok(r["text"].as_str().unwrap_or("").to_string())
    }

    /// The tab's transcript as `{agent, messages}` when it changed since
    /// `stamp`, with the new stamp. The stamp is opaque to callers.
    pub async fn chat(&self, surface: &str, stamp: Option<&str>) -> Option<(String, Value)> {
        let tabs = self.tree.lock().await.last.as_ref().map(|(_, _, t)| t.clone())?;
        let tab = tabs.get(surface)?;
        let (agent, path) = (tab.agent?, tab.transcript.clone()?);
        let meta = tokio::fs::metadata(&path).await.ok()?;
        let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
        let new = format!("{mtime}:{}:{}", meta.len(), path.display());
        if stamp == Some(new.as_str()) {
            return None;
        }
        let messages = tokio::task::spawn_blocking(move || agents::read_chat(agent, &path)).await.ok()?.ok()?;
        Some((new, json!({"agent": agent, "messages": messages})))
    }

    /// The daemon's notifications, as the daemon lists them.
    pub async fn notifications(&self) -> anyhow::Result<Vec<Value>> {
        let r = rpc(&self.socket, "notify.list", json!({})).await?;
        Ok(r["notifications"].as_array().cloned().unwrap_or_default())
    }

    pub async fn send(&self, surface: &str, text: &str) -> anyhow::Result<()> {
        rpc(&self.socket, "term.send", json!({"surface": surface, "text": text})).await.map(drop)
    }

    pub async fn key(&self, surface: &str, key: &str) -> anyhow::Result<()> {
        rpc(&self.socket, "term.send_key", json!({"surface": surface, "key": key})).await.map(drop)
    }

    /// Saves a finished upload and returns its path on this machine.
    pub async fn save(&self, p: Pending) -> anyhow::Result<String> {
        let dir = self.uploads.clone();
        let path = tokio::task::spawn_blocking(move || p.save(&dir)).await??;
        Ok(path.to_string_lossy().into_owned())
    }
}

/// A machine whose sessions the share serves.
pub enum Host {
    Local(Arc<Local>),
    Remote(Arc<Remote>),
}

impl Host {
    pub async fn tree(&self) -> anyhow::Result<Value> {
        match self {
            Host::Local(l) => l.tree().await,
            Host::Remote(r) => r.call(json!({"op": "tree"})).await,
        }
    }

    pub async fn screen(&self, surface: &str) -> anyhow::Result<String> {
        match self {
            Host::Local(l) => l.screen(surface).await,
            Host::Remote(r) => {
                let v = r.call(json!({"op": "screen", "surface": surface})).await?;
                Ok(v["text"].as_str().unwrap_or("").to_string())
            }
        }
    }

    pub async fn chat(&self, surface: &str, stamp: Option<&str>) -> Option<(String, Value)> {
        match self {
            Host::Local(l) => l.chat(surface, stamp).await,
            Host::Remote(r) => {
                let v = r.call(json!({"op": "chat", "surface": surface, "stamp": stamp})).await.ok()?;
                let stamp = v["stamp"].as_str()?.to_string();
                Some((stamp, v["chat"].clone()))
            }
        }
    }

    pub async fn notifications(&self) -> anyhow::Result<Vec<Value>> {
        match self {
            Host::Local(l) => l.notifications().await,
            Host::Remote(r) => {
                let v = r.call(json!({"op": "notifications"})).await?;
                Ok(v["notifications"].as_array().cloned().unwrap_or_default())
            }
        }
    }

    pub async fn send(&self, surface: &str, text: &str) -> anyhow::Result<()> {
        match self {
            Host::Local(l) => l.send(surface, text).await,
            Host::Remote(r) => r.call(json!({"op": "send", "surface": surface, "text": text})).await.map(drop),
        }
    }

    pub async fn key(&self, surface: &str, key: &str) -> anyhow::Result<()> {
        match self {
            Host::Local(l) => l.key(surface, key).await,
            Host::Remote(r) => r.call(json!({"op": "key", "surface": surface, "key": key})).await.map(drop),
        }
    }

    pub async fn save(&self, p: Pending) -> anyhow::Result<String> {
        match self {
            Host::Local(l) => l.save(p).await,
            Host::Remote(r) => {
                use base64::Engine;
                let (name, data) = p.into_parts();
                let data = base64::engine::general_purpose::STANDARD.encode(data);
                let v = r.call(json!({"op": "upload", "name": name, "data": data})).await?;
                v["path"].as_str().map(str::to_owned).context("no path")
            }
        }
    }
}

/// The slim tree the page needs: workspaces with their tabs flattened, and
/// what is known about the agent in each terminal tab.
pub fn tree_to_message(tree: &Value, agents: &std::collections::HashMap<String, TabAgent>) -> Value {
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
    json!({"host": tree["host"], "workspaces": workspaces})
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
