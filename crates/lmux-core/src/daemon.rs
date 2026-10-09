//! The daemon: owns the session tree, drives tmux, persists state and serves the
//! unix-socket RPC channel and the app websocket.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use lmux_proto::frame::Frame;
use lmux_proto::*;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;

use crate::browser::BrowserBackend;
use crate::scrollback;
use crate::tmux::{Tmux, TmuxConfig, TmuxEvent};
use crate::tmux::WinInfo;
use crate::tree::{terminal_surface, AgentSession, SessionState, TermMeta};

#[derive(Debug, Clone)]
pub struct Config {
    pub state_dir: PathBuf,
    /// The unix socket to listen on; `<state_dir>/lmux.sock` when unset.
    pub socket: Option<PathBuf>,
    pub tmux_socket: String,
    pub ws_addr: String,
    pub host: String,
    /// Browser panes are drawn and run by the app (WebKit), with no Chromium
    /// here: the daemon keeps their URL and title so they come back.
    pub app_browser: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            state_dir: crate::default_state_dir(),
            socket: None,
            tmux_socket: "lmux".to_string(),
            ws_addr: crate::default_ws_addr(),
            host: hostname(),
            app_browser: false,
        }
    }
}

impl Config {
    pub fn socket_path(&self) -> PathBuf {
        self.socket.clone().unwrap_or_else(|| self.state_dir.join("lmux.sock"))
    }
    pub fn state_path(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Branch labels are re-derived at most this often per cwd. Short enough to pick
/// up a checkout, long enough that a burst of tree sends is one `git` call.
const BRANCH_TTL: Duration = Duration::from_secs(3);

/// How long to coalesce tree changes before sending. Bounds the send rate while
/// a window edge is being dragged (which fires many resizes).
const TREE_COALESCE: Duration = Duration::from_millis(50);

/// Background branch-refresh cadence. Resizes never run `git`.
const BRANCH_REFRESH: Duration = Duration::from_secs(5);
/// How often listening ports and pull requests are looked up for the cards.
const PROBE_REFRESH: Duration = Duration::from_secs(10);
/// How long a workspace's pull request lookup (`gh`) is reused.
const PR_TTL: Duration = Duration::from_secs(60);

/// Persist debounce: coalesce a burst of changes into one write.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);

/// How often live panes are checked for cwd changes and exited agents.
const RESUME_REFRESH: Duration = Duration::from_secs(30);

/// How often changed terminals have their scrollback saved to disk.
const SCROLLBACK_INTERVAL: Duration = RESUME_REFRESH;

/// Bound on the final scrollback capture at shutdown (tmux may already be gone).
const SHUTDOWN_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// After typing `claude --resume`, the pane shows a shell until Claude starts.
/// Within this window a shell foreground does not mean "Claude exited".
const RESUME_GRACE: Duration = Duration::from_secs(30);

/// Pause between resumed agents, so ten panes do not start Claude at once.
const RESUME_STAGGER: Duration = Duration::from_millis(750);

/// Per-client queue cap (messages). Past this the client is lagging; we drop its
/// backlog rather than growing without bound while the laptop is asleep.
const CLIENT_QUEUE_MAX: usize = 512;

/// True for the literal `HEAD` that `rev-parse --abbrev-ref` prints on a detached
/// HEAD; the caller falls back to the short commit.
fn parse_branch_output(stdout: &str) -> Option<String> {
    let t = stdout.trim();
    if t.is_empty() || t == "HEAD" {
        None
    } else {
        Some(t.to_string())
    }
}

/// `git -C <cwd> rev-parse --abbrev-ref HEAD`, falling back to the short commit on
/// a detached HEAD. `None` for a non-repo cwd, a missing `git`, or any error.
/// Shelling out beats adding a `git2` dependency: one short-lived process per
/// distinct cwd per TTL, no native build, and it uses the user's git config.
pub async fn git_branch(cwd: &str) -> Option<String> {
    let out = git_quick(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if let Some(b) = parse_branch_output(&out) {
        return Some(b);
    }
    // Detached HEAD: abbrev-ref printed "HEAD"; show the short commit instead.
    parse_branch_output(&git_quick(cwd, &["rev-parse", "--short", "HEAD"]).await?)
}

/// Run git in `cwd`, giving up after a few seconds. On macOS a folder such as
/// ~/Documents can hold git until the user answers a privacy prompt, and a
/// workspace create waits for its branch, so git must never hang the daemon.
async fn git_quick(cwd: &str, args: &[&str]) -> Option<String> {
    let run = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(3), run).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A message to one websocket client.
#[derive(Debug, Clone)]
pub enum Out {
    Msg(ServerMsg),
    /// Pre-encoded binary frame, shared cheaply across subscribers.
    Bytes(Bytes),
}

/// A bounded, clearable per-client outbox. The WS writer task drains it; when the
/// producer overflows it the client is lagging, so we drop the backlog and
/// resynchronise instead of buffering forever.
struct ClientSink {
    queue: std::sync::Mutex<VecDeque<Out>>,
    notify: Notify,
    closed: AtomicBool,
    resync_pending: AtomicBool,
    max: usize,
}

impl ClientSink {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(ClientSink {
            queue: std::sync::Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            resync_pending: AtomicBool::new(false),
            max,
        })
    }

    /// Returns false if the queue is at capacity (caller should recover).
    fn push(&self, out: Out) -> bool {
        if self.closed.load(Ordering::Relaxed) {
            return true; // drop silently; nobody is reading
        }
        let mut q = self.queue.lock().unwrap();
        if q.len() >= self.max {
            return false;
        }
        q.push_back(out);
        drop(q);
        self.notify.notify_one();
        true
    }

    fn take_all(&self) -> Vec<Out> {
        let mut q = self.queue.lock().unwrap();
        q.drain(..).collect()
    }

    /// Drop everything queued (used when a client has fallen behind).
    fn clear(&self) {
        self.queue.lock().unwrap().clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
}

#[derive(Default)]
struct Clients {
    conns: HashMap<u64, Arc<ClientSink>>,
    /// Connections that are lmux apps (they said so in `hello`), oldest
    /// first: they draw in-app browsers and run steps in them.
    apps: Vec<u64>,
    /// surface -> connection ids subscribed.
    subs: HashMap<Ref, HashSet<u64>>,
    next_id: u64,
}

/// Coalesces a burst of tree changes into one send. `arm` starts the window on
/// the first mark; further marks do not extend it.
struct Coalescer {
    window: Duration,
    deadline: Option<Instant>,
}

impl Coalescer {
    fn new(window: Duration) -> Self {
        Coalescer {
            window,
            deadline: None,
        }
    }

    fn arm(&mut self) {
        if self.deadline.is_none() {
            self.deadline = Some(Instant::now() + self.window);
        }
    }

    fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Returns true (and resets) once the window has elapsed.
    fn fire(&mut self) -> bool {
        match self.deadline {
            Some(d) if Instant::now() >= d => {
                self.deadline = None;
                true
            }
            _ => false,
        }
    }
}

pub struct Inner {
    pub cfg: Config,
    /// The whole session (tree + tmux indexes + branch cache) under one lock.
    pub state: Mutex<SessionState>,
    pub tmux: Tmux,
    pub browser: Option<Arc<dyn BrowserBackend>>,
    /// Surfaces whose screencast is currently running.
    browser_streaming: Mutex<HashSet<Ref>>,
    /// The size each browser was last shown at, so a stream restarted for a
    /// viewer that reconnects (and does not resend its size) matches its canvas.
    browser_sizes: std::sync::Mutex<HashMap<Ref, (u32, u32, f32)>>,
    clients: Mutex<Clients>,
    /// Wakes the persist task.
    dirty: Notify,
    /// Wakes the tree-coalescing task.
    tree_dirty: Notify,
    /// Respawn bookkeeping (see `reconcile`).
    respawn: std::sync::Mutex<RespawnState>,
    /// Terminals with output since their scrollback was last saved.
    scrollback_dirty: std::sync::Mutex<HashSet<Ref>>,
    /// Pull request lookups by `cwd\0branch`, with when they were made.
    prs: std::sync::Mutex<HashMap<String, (Option<lmux_proto::PullRequest>, Instant)>>,
    /// Steps sent to an app for its in-app browsers, awaiting answers.
    pub(crate) app_calls: std::sync::Mutex<crate::app_browser::AppCalls>,
}

#[derive(Default)]
struct RespawnState {
    /// Surfaces with a respawn in flight, so a second reconcile skips them.
    in_flight: HashSet<Ref>,
    /// When an agent was last typed into a respawned pane (see `RESUME_GRACE`).
    resumed_at: HashMap<Ref, Instant>,
}

/// A terminal whose tmux pane is gone and must be started again.
struct Respawn {
    surface: Ref,
    workspace: Ref,
    title: String,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    workspace_cwd: String,
    agent: Option<AgentSession>,
}

/// Foreground command names that mean "only the shell is running".
fn is_shell(command: &str) -> bool {
    matches!(
        command.trim_start_matches('-'),
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh" | "nu"
    )
}

/// Parse a tmux `@lmux_agent_session` value, `<agent>:<id>`.
fn parse_agent_tag(tag: &str) -> Option<AgentSession> {
    let (agent, id) = tag.split_once(':')?;
    (matches!(agent, "claude" | "codex") && crate::tree::valid_agent_session_id(id)).then(|| AgentSession {
        agent: agent.to_string(),
        id: id.to_string(),
    })
}

/// The command typed into a respawned pane to resume an agent.
fn resume_command(a: &AgentSession) -> Option<String> {
    if !crate::tree::valid_agent_session_id(&a.id) {
        return None;
    }
    match a.agent.as_str() {
        "claude" => Some(format!("claude --resume {}\r", a.id)),
        "codex" => Some(format!("codex resume {}\r", a.id)),
        _ => None,
    }
}

impl Inner {
    pub async fn tree(&self) -> Tree {
        self.state.lock().await.to_tree()
    }

    // --- clients ---------------------------------------------------------

    async fn register(&self, sink: Arc<ClientSink>) -> u64 {
        let mut c = self.clients.lock().await;
        c.next_id += 1;
        let id = c.next_id;
        c.conns.insert(id, sink);
        id
    }

    /// Forget a client. A browser it was the last viewer of stops streaming,
    /// as on `Unsubscribe`: its last frame was never acked, so a stream left
    /// running would stay stalled for the next viewer.
    async fn unregister(&self, id: u64) {
        let orphaned: Vec<Ref> = {
            let mut c = self.clients.lock().await;
            c.conns.remove(&id);
            c.apps.retain(|a| *a != id);
            c.subs
                .iter_mut()
                .filter_map(|(surface, set)| (set.remove(&id) && set.is_empty()).then(|| surface.clone()))
                .collect()
        };
        for surface in orphaned {
            if self.is_streaming(&surface).await {
                self.clear_streaming(&surface).await;
                if let Some(b) = &self.browser {
                    let _ = b.stop_screencast(&surface).await;
                }
            }
        }
    }

    /// Send a step to the app that draws `surface` (the newest app connected),
    /// returning the call id and where its answer will arrive.
    pub(crate) async fn call_app(
        &self,
        surface: &str,
        op: AppBrowserOp,
    ) -> Result<(u64, tokio::sync::oneshot::Receiver<Result<serde_json::Value, String>>), String> {
        let sink = {
            let c = self.clients.lock().await;
            c.apps.iter().rev().find_map(|a| c.conns.get(a).cloned())
        };
        let Some(sink) = sink else {
            return Err("this browser runs in the lmux app, and no lmux app is connected".into());
        };
        let (id, rx) = self.app_calls.lock().unwrap().start();
        if !sink.push(Out::Msg(ServerMsg::AppBrowserCall { id, surface: surface.to_owned(), op })) {
            self.app_calls.lock().unwrap().forget(id);
            return Err("the lmux app is not keeping up".into());
        }
        Ok((id, rx))
    }

    async fn add_subscription(&self, id: u64, surface: &str) -> usize {
        let mut c = self.clients.lock().await;
        let set = c.subs.entry(surface.to_string()).or_default();
        set.insert(id);
        set.len()
    }

    async fn remove_subscription(&self, id: u64, surface: &str) -> usize {
        let mut c = self.clients.lock().await;
        let set = c.subs.entry(surface.to_string()).or_default();
        set.remove(&id);
        set.len()
    }

    async fn subscriber_count(&self, surface: &str) -> usize {
        let c = self.clients.lock().await;
        c.subs.get(surface).map(|s| s.len()).unwrap_or(0)
    }

    async fn has_subscribers(&self, surface: &str) -> bool {
        let c = self.clients.lock().await;
        c.subs.get(surface).map(|s| !s.is_empty()).unwrap_or(false)
    }

    /// Send to every conn subscribed to `surface`; recover any that overflow.
    /// With no subscribers this costs one map lookup and builds nothing.
    pub async fn send_to_surface(self: &Arc<Self>, surface: &str, out: Out) {
        let targets: Vec<(u64, Arc<ClientSink>)> = {
            let c = self.clients.lock().await;
            let Some(ids) = c.subs.get(surface) else {
                return;
            };
            ids.iter()
                .filter_map(|id| c.conns.get(id).map(|s| (*id, s.clone())))
                .collect()
        };
        for (id, sink) in targets {
            if !sink.push(out.clone()) {
                self.recover_client(id, Some(surface)).await;
            }
        }
    }

    pub async fn broadcast(self: &Arc<Self>, out: Out) {
        let targets: Vec<(u64, Arc<ClientSink>)> = {
            let c = self.clients.lock().await;
            c.conns
                .iter()
                .map(|(id, s)| (*id, s.clone()))
                .collect()
        };
        for (id, sink) in targets {
            if !sink.push(out.clone()) {
                self.recover_client(id, None).await;
            }
        }
    }

    /// Clear a lagging client's backlog, resend it a fresh tree, and (when we
    /// know the surface) schedule a screen snapshot so it jumps to now instead of
    /// replaying every missed byte.
    async fn recover_client(self: &Arc<Self>, id: u64, surface: Option<&str>) {
        {
            let c = self.clients.lock().await;
            if let Some(sink) = c.conns.get(&id) {
                sink.clear();
            } else {
                return;
            }
        }
        if let Some(surface) = surface {
            let maybe = {
                let c = self.clients.lock().await;
                c.conns.get(&id).cloned()
            };
            if let Some(sink) = maybe {
                if !sink.resync_pending.swap(true, Ordering::AcqRel) {
                    let me = self.clone();
                    let surface = surface.to_string();
                    tokio::spawn(async move {
                        me.resync_surface(id, &surface, &sink).await;
                        sink.resync_pending.store(false, Ordering::Release);
                    });
                }
            }
        }
        // A fresh tree re-establishes the client's view.
        let tree = self.tree().await;
        self.push_to_client(id, Out::Msg(ServerMsg::Tree { tree })).await;
    }

    async fn resync_surface(self: &Arc<Self>, id: u64, surface: &str, sink: &Arc<ClientSink>) {
        let term = { self.state.lock().await.term_cloned(surface) };
        let Some(term) = term else { return };
        match self.tmux.snapshot(&term.pane).await {
            Ok((cols, rows, data)) => {
                // Drop anything queued during the capture, then send the snapshot.
                sink.clear();
                self.push_to_client(
                    id,
                    Out::Msg(ServerMsg::TermSnapshot {
                        surface: surface.to_string(),
                        cols,
                        rows,
                        data,
                    }),
                )
                .await;
            }
            Err(e) => tracing::debug!("resync snapshot failed: {e}"),
        }
    }

    async fn push_to_client(self: &Arc<Self>, id: u64, out: Out) {
        let sink = {
            let c = self.clients.lock().await;
            c.conns.get(&id).cloned()
        };
        if let Some(sink) = sink {
            sink.push(out);
        }
    }

    // --- tree / branches -------------------------------------------------

    /// Mark the tree as changed; a single task sends it at most once per window.
    pub fn tree_changed(&self) {
        self.tree_dirty.notify_one();
    }

    pub fn mark_dirty(&self) {
        self.dirty.notify_one();
    }

    /// Branch for `cwd`, cached for `BRANCH_TTL`.
    async fn detect_branch(&self, cwd: &str) -> Option<String> {
        let cached = {
            let s = self.state.lock().await;
            s.branches.get(cwd).cloned()
        };
        if let Some((branch, at)) = cached {
            if at.elapsed() < BRANCH_TTL {
                return branch;
            }
        }
        let branch = git_branch(cwd).await;
        self.state
            .lock()
            .await
            .branches
            .insert(cwd.to_string(), (branch.clone(), Instant::now()));
        branch
    }

    /// Set one workspace's `git_branch` now, so a new workspace shows its
    /// branch at once without waiting on every other workspace's git.
    pub async fn refresh_branch_of(&self, workspace: &str) {
        let cwd = {
            let state = self.state.lock().await;
            state.workspace(workspace).map(|w| w.cwd.clone())
        };
        let Some(cwd) = cwd.filter(|c| !c.is_empty()) else { return };
        let branch = self.detect_branch(&cwd).await;
        if let Some(w) = self.state.lock().await.workspace_mut(workspace) {
            w.git_branch = branch;
        }
    }

    /// Refresh every workspace's `git_branch`. Returns true if anything changed.
    pub async fn refresh_branches(&self) -> bool {
        let pairs: Vec<(Ref, String)> = {
            let state = self.state.lock().await;
            state
                .workspaces
                .iter()
                .map(|w| (w.id.clone(), w.cwd.clone()))
                .collect()
        };
        let mut changed = false;
        for (id, cwd) in pairs {
            if cwd.is_empty() {
                continue;
            }
            let branch = self.detect_branch(&cwd).await;
            let mut state = self.state.lock().await;
            if let Some(w) = state.workspace_mut(&id) {
                if w.git_branch != branch {
                    w.git_branch = branch;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Refresh every workspace's listening ports and pull request. Returns
    /// true if anything changed.
    pub async fn refresh_probes(&self) -> bool {
        // Each pane's shell pid, from tmux.
        let pids = self.tmux.pane_pids().await.unwrap_or_default();
        let (roots, repos): (HashMap<String, Vec<u32>>, Vec<(Ref, String, String)>) = {
            let state = self.state.lock().await;
            let roots = state
                .workspaces
                .iter()
                .map(|w| {
                    let pids = w
                        .panes
                        .iter()
                        .flat_map(|p| p.surfaces.iter())
                        .filter_map(|s| state.term_cloned(&s.id))
                        .filter_map(|t| pids.get(&t.pane).copied())
                        .collect();
                    (w.id.clone(), pids)
                })
                .collect();
            let repos = state
                .workspaces
                .iter()
                .filter_map(|w| w.git_branch.clone().map(|b| (w.id.clone(), w.cwd.clone(), b)))
                .collect();
            (roots, repos)
        };
        let ports = crate::probe::ports_by_workspace(&roots).await;

        // Pull requests, one `gh` per repo and branch per PR_TTL.
        let mut prs: HashMap<Ref, Option<lmux_proto::PullRequest>> = HashMap::new();
        for (ws, cwd, branch) in repos {
            let key = format!("{cwd}\0{branch}");
            let cached = self.prs.lock().unwrap().get(&key).cloned();
            let pr = match cached {
                Some((pr, at)) if at.elapsed() < PR_TTL => pr,
                _ => {
                    let pr = crate::probe::pull_request(&cwd).await;
                    self.prs.lock().unwrap().insert(key, (pr.clone(), Instant::now()));
                    pr
                }
            };
            prs.insert(ws, pr);
        }

        let mut changed = false;
        let mut state = self.state.lock().await;
        for w in state.workspaces.iter_mut() {
            let p = ports.get(&w.id).cloned().unwrap_or_default();
            if w.ports != p {
                w.ports = p;
                changed = true;
            }
            let pr = prs.remove(&w.id).flatten();
            if w.pr != pr {
                w.pr = pr;
                changed = true;
            }
        }
        changed
    }

    pub async fn surface_for_pane(&self, pane: &str) -> Option<Ref> {
        self.state.lock().await.surface_of_pane(pane)
    }

    pub async fn surface_term(&self, surface: &str) -> Option<TermMeta> {
        self.state.lock().await.term_cloned(surface)
    }

    pub async fn persist(&self) {
        let bytes = {
            let s = self.state.lock().await;
            match serde_json::to_vec(&*s) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("serialising state: {e}");
                    return;
                }
            }
        };
        let path = self.cfg.state_path();
        // Single writer task, so one temp path is safe; rename keeps it atomic.
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = tokio::fs::write(&tmp, &bytes).await {
            tracing::warn!("writing state: {e}");
            return;
        }
        if let Err(e) = tokio::fs::rename(&tmp, &path).await {
            tracing::warn!("renaming state: {e}");
        }
    }

    // --- browser streaming ----------------------------------------------

    async fn mark_streaming(&self, surface: &str) {
        self.browser_streaming
            .lock()
            .await
            .insert(surface.to_string());
    }

    async fn is_streaming(&self, surface: &str) -> bool {
        self.browser_streaming.lock().await.contains(surface)
    }

    async fn clear_streaming(&self, surface: &str) {
        self.browser_streaming.lock().await.remove(surface);
    }

    /// Reconcile the loaded state with the still-running tmux server. Takes only
    /// the single state lock.
    async fn reconcile(self: &Arc<Self>) {
        let wins = match self.tmux.list_windows().await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("reconcile: list-windows failed: {e}");
                return;
            }
        };
        let live: HashSet<String> = wins.iter().map(|w| w.pane.clone()).collect();
        let has_browser = self.browser.is_some();
        let mut respawn: Vec<Respawn> = Vec::new();
        {
            let mut state = self.state.lock().await;
            let in_flight = self.respawn.lock().unwrap().in_flight.clone();

            // 1. Refresh live terminals. A terminal whose tmux pane is gone was
            //    not closed by the user (that path is `handle_window_closed`):
            //    tmux itself went away, so start it again rather than drop it.
            let surfaces = state.all_surface_refs();
            for surf in surfaces {
                let kind = state.surface(&surf).map(|s| (s.kind.clone(), s.title.clone()));
                match kind {
                    Some((SurfaceKind::Terminal { tmux_pane, cols, rows }, title)) => match tmux_pane {
                        Some(p) if live.contains(&p) => {
                            if let Some(w) = wins.iter().find(|w| w.pane == p) {
                                state.set_pane_surface_size(&surf, w.cols, w.rows);
                                state.set_term(&surf, &w.window, &p);
                                self.observe_live(&mut state, &surf, w);
                            }
                        }
                        _ => {
                            if in_flight.contains(&surf) {
                                continue;
                            }
                            let Some(ws) = state.workspace_of_surface(&surf) else {
                                continue;
                            };
                            let workspace_cwd = state
                                .workspace(&ws)
                                .map(|w| w.cwd.clone())
                                .unwrap_or_default();
                            let r = state.resume.get(&surf).cloned().unwrap_or_default();
                            respawn.push(Respawn {
                                surface: surf.clone(),
                                workspace: ws,
                                title,
                                cols,
                                rows,
                                cwd: r.cwd,
                                workspace_cwd,
                                agent: r.agent,
                            });
                        }
                    },
                    Some((SurfaceKind::Browser { .. }, _)) => {
                        if !has_browser {
                            state.remove_surface(&surf);
                        }
                    }
                    None => {}
                }
            }

            // 2. Adopt tagged windows the file did not know about.
            for w in &wins {
                let Some(surf) = w.surface.clone() else {
                    continue;
                };
                if state.surface(&surf).is_some() {
                    continue;
                }
                let ws_ref = w
                    .workspace
                    .clone()
                    .unwrap_or_else(|| state.next_workspace());
                if state.workspace(&ws_ref).is_none() {
                    let id = ws_ref.clone();
                    state.next_workspace(); // keep counter ahead of adopted ids
                    state.observe_ref(&id);
                    state.add_workspace(Workspace {
                        id: id.clone(),
                        title: w.title.clone().unwrap_or_else(|| "adopted".to_string()),
                        folder: None,
                        cwd: std::env::var("HOME").unwrap_or_else(|_| "/".into()),
                        git_branch: None,
                        agent: AgentState::None,
                        summary: None,
                        unread_notifications: 0,
                        status_pills: vec![],
                        progress: None,
                        layout: Layout::Pane {
                            pane: String::new(),
                        },
                        panes: vec![],
                        focused_pane: None,
                        ports: vec![],
                        pr: None,
                    });
                }
                state.observe_ref(&surf);
                let pane_ref = state.next_pane();
                let mut surface = terminal_surface(
                    surf.clone(),
                    w.title.clone().unwrap_or_else(|| "term".to_string()),
                    w.cols,
                    w.rows,
                );
                if let SurfaceKind::Terminal { tmux_pane, .. } = &mut surface.kind {
                    *tmux_pane = Some(w.pane.clone());
                }
                let pane = Pane {
                    id: pane_ref.clone(),
                    surfaces: vec![surface],
                    active_surface: Some(surf.clone()),
                };
                let first = {
                    let ws = state.workspace_mut(&ws_ref).unwrap();
                    let empty = ws.panes.is_empty();
                    let first = ws.panes.first().map(|p| p.id.clone());
                    if empty {
                        ws.layout = Layout::Pane {
                            pane: pane_ref.clone(),
                        };
                    }
                    ws.panes.push(pane);
                    first
                };
                if let Some(first) = first {
                    let _ = state.split_pane(&ws_ref, &first, SplitSide::Right, &pane_ref);
                }
                state.workspace_mut(&ws_ref).unwrap().focused_pane = Some(pane_ref.clone());
                state.set_term(&surf, &w.window, &w.pane);
                self.observe_live(&mut state, &surf, w);
            }

            if state.selected_workspace.is_none() {
                state.selected_workspace = state.workspaces.first().map(|w| w.id.clone());
            }
            state.prune_resume();
        }
        tracing::info!(windows = wins.len(), respawn = respawn.len(), "reconciled with tmux");
        if !respawn.is_empty() {
            {
                let mut r = self.respawn.lock().unwrap();
                for x in &respawn {
                    r.in_flight.insert(x.surface.clone());
                }
            }
            // In the background: starting ten agents one after another must not
            // hold up the app websocket.
            let me = self.clone();
            tokio::spawn(async move { me.respawn_terminals(respawn).await });
        }
        self.mark_dirty();
    }

    /// Open the browser tabs the state file kept, at their saved URLs. The
    /// browser starts empty with the daemon: without this a restored browser
    /// pane is in the tree but has no page, so it never shows a frame.
    async fn reopen_browsers(self: &Arc<Self>) {
        // Tabs saved under the other kind of browser (Chromium here, or the
        // app's WebKit) switch to this one, rather than waiting for a page
        // nothing will draw.
        if self.browser.is_some() || self.cfg.app_browser {
            let in_app_now = self.browser.is_none();
            let mut changed = false;
            let mut state = self.state.lock().await;
            for surf in state.all_surface_refs() {
                if let Some(SurfaceKind::Browser { in_app, .. }) = state.surface_mut(&surf).map(|s| &mut s.kind) {
                    changed |= *in_app != in_app_now;
                    *in_app = in_app_now;
                }
            }
            drop(state);
            if changed {
                self.tree_changed();
                self.mark_dirty();
            }
        }
        let Some(b) = self.browser.clone() else {
            return;
        };
        let browsers: Vec<(Ref, String)> = {
            let state = self.state.lock().await;
            state
                .all_surface_refs()
                .into_iter()
                .filter_map(|surf| match state.surface(&surf).map(|s| &s.kind) {
                    Some(SurfaceKind::Browser { url, .. }) => Some((surf, url.clone())),
                    _ => None,
                })
                .collect()
        };
        for (surf, url) in browsers {
            if let Err(e) = b.create_surface(&surf, Some(&url)).await {
                tracing::warn!(surface = %surf, "reopening browser failed: {e}");
                continue;
            }
            // A viewer that subscribed before its page was back got no stream.
            if self.has_subscribers(&surf).await {
                self.clear_streaming(&surf).await;
                self.maybe_start_fallback_screencast(surf);
            }
        }
    }

    /// Record a live pane's cwd, pick up an agent tag the state file lost, and
    /// forget an agent that has exited (its shell is back in the foreground).
    fn observe_live(&self, state: &mut SessionState, surf: &str, w: &WinInfo) {
        let in_grace = self
            .respawn
            .lock()
            .unwrap()
            .resumed_at
            .get(surf)
            .is_some_and(|t| t.elapsed() < RESUME_GRACE);
        let r = state.resume_mut(surf);
        if let Some(cwd) = &w.cwd {
            r.cwd = Some(cwd.clone());
        }
        if r.agent.is_none() {
            r.agent = w.agent_session.as_deref().and_then(parse_agent_tag);
        }
        if r.agent.is_some() && !in_grace && w.command.as_deref().is_some_and(is_shell) {
            // Claude was quit on purpose: a reboot must not bring it back.
            r.agent = None;
            let me_tmux = self.tmux.clone();
            let window = w.window.clone();
            tokio::spawn(async move {
                let _ = me_tmux.set_window_option(&window, "@lmux_agent_session", "").await;
            });
        }
    }

    /// Periodic cwd / agent-exit refresh for live panes.
    async fn refresh_resume(self: &Arc<Self>) {
        let Ok(wins) = self.tmux.list_windows().await else {
            return;
        };
        // Codex has no session hook: find the rollout file each pane's codex
        // process has open, and tag the window like Claude's hook does.
        let pids = self.tmux.pane_pids().await.unwrap_or_default();
        for (pane, id) in crate::probe::codex_sessions(&pids).await {
            let Some(w) = wins.iter().find(|w| w.pane == pane) else { continue };
            let tag = format!("codex:{id}");
            if w.agent_session.as_deref() != Some(tag.as_str()) {
                let _ = self.tmux.set_window_option(&w.window, "@lmux_agent_session", &tag).await;
                let mut state = self.state.lock().await;
                if let Some(surf) = state.pane_index.get(&pane).cloned() {
                    state.resume_mut(&surf).agent = Some(AgentSession { agent: "codex".into(), id });
                }
            }
        }
        let before;
        let after;
        {
            let mut state = self.state.lock().await;
            before = state.resume.clone();
            for w in &wins {
                if let Some(surf) = state.pane_index.get(&w.pane).cloned() {
                    self.observe_live(&mut state, &surf, w);
                }
            }
            after = state.resume.clone();
        }
        if before != after {
            self.mark_dirty();
        }
    }

    /// Start lost terminals again in their old directories, typing the agent's
    /// resume command where one was running. Each agent session resumes in at
    /// most one pane; duplicates get a plain shell.
    async fn respawn_terminals(self: Arc<Self>, list: Vec<Respawn>) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut resumed = 0usize;
        let mut missing_dirs: Vec<String> = Vec::new();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let is_dir = |d: &str| !d.is_empty() && std::path::Path::new(d).is_dir();
        for r in list {
            let saved_dir_gone = r.cwd.as_deref().is_some_and(|d| !is_dir(d));
            let cwd = r
                .cwd
                .clone()
                .filter(|d| is_dir(d))
                .or_else(|| Some(r.workspace_cwd.clone()).filter(|d| is_dir(d)))
                .unwrap_or_else(|| home.clone());
            let agent = r
                .agent
                .clone()
                .filter(|_| !saved_dir_gone)
                .filter(|a| resume_command(a).is_some() && seen.insert(a.id.clone()));
            // Replay the saved screen: the window's first process prints a
            // private copy of it, then execs the login shell in `cwd`. A pane
            // that resumes an agent skips it: the agent redraws its own
            // conversation, so the replay would show it twice.
            let restore = match agent {
                Some(_) => None,
                None => scrollback::stage_restore(&self.cfg.state_dir, &r.surface).await,
            };
            let (command, env) = match &restore {
                Some(p) => (
                    Some(scrollback::restore_command()),
                    vec![(
                        scrollback::RESTORE_ENV.to_string(),
                        p.to_string_lossy().into_owned(),
                    )],
                ),
                None => (None, vec![]),
            };
            let spawned = crate::rpc::spawn_terminal_env(
                &self,
                &r.workspace,
                &r.surface,
                &r.title,
                &cwd,
                command.as_deref(),
                &env,
                r.cols,
                r.rows,
            )
            .await;
            let (window, pane) = match spawned {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(surface = %r.surface, "respawn failed: {e}");
                    if let Some(p) = &restore {
                        let _ = tokio::fs::remove_file(p).await;
                    }
                    self.respawn.lock().unwrap().in_flight.remove(&r.surface);
                    continue;
                }
            };
            match agent {
                Some(a) => {
                    // Let the shell come up before typing into it.
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let cmd = resume_command(&a).unwrap_or_default();
                    if let Err(e) = self.tmux.send_bytes(&pane, cmd.as_bytes()).await {
                        tracing::warn!(surface = %r.surface, "typing resume failed: {e}");
                    }
                    let _ = self
                        .tmux
                        .set_window_option(&window, "@lmux_agent_session", &format!("{}:{}", a.agent, a.id))
                        .await;
                    self.respawn
                        .lock()
                        .unwrap()
                        .resumed_at
                        .insert(r.surface.clone(), Instant::now());
                    resumed += 1;
                    tracing::info!(surface = %r.surface, session = %a.id, "resumed agent");
                    tokio::time::sleep(RESUME_STAGGER).await;
                }
                None => {
                    if saved_dir_gone && r.agent.is_some() {
                        missing_dirs.push(r.title.clone());
                    }
                    let mut state = self.state.lock().await;
                    state.resume_mut(&r.surface).agent = None;
                }
            }
            self.respawn.lock().unwrap().in_flight.remove(&r.surface);
            self.resnapshot_subscribers(&r.surface).await;
            self.tree_changed();
        }
        self.mark_dirty();
        let mut lines = Vec::new();
        if resumed > 0 {
            lines.push(format!(
                "Resumed {resumed} Claude session{} after tmux restarted.",
                if resumed == 1 { "" } else { "s" }
            ));
        }
        if !missing_dirs.is_empty() {
            lines.push(format!(
                "Could not resume in {}: saved directory is gone.",
                missing_dirs.join(", ")
            ));
        }
        if !lines.is_empty() {
            let req = RpcRequest {
                id: 0,
                method: methods::NOTIFY.to_string(),
                params: json!({"title": "lmux", "body": lines.join(" ")}),
            };
            crate::rpc::dispatch(&self, &req).await;
        }
    }

    /// Clients still subscribed to a respawned terminal hold the old screen;
    /// send each a snapshot of the new pane.
    async fn resnapshot_subscribers(self: &Arc<Self>, surface: &str) {
        let ids: Vec<u64> = {
            let c = self.clients.lock().await;
            c.subs.get(surface).map(|s| s.iter().copied().collect()).unwrap_or_default()
        };
        if ids.is_empty() {
            return;
        }
        let Some(term) = self.state.lock().await.term_cloned(surface) else {
            return;
        };
        let Ok((cols, rows, data)) = self.tmux.snapshot(&term.pane).await else {
            return;
        };
        for id in ids {
            self.push_to_client(
                id,
                Out::Msg(ServerMsg::TermSnapshot {
                    surface: surface.to_string(),
                    cols,
                    rows,
                    data: data.clone(),
                }),
            )
            .await;
        }
    }

    // --- scrollback -------------------------------------------------------

    /// Note output on a terminal, so the next capture saves it.
    fn note_output(&self, surface: &str) {
        let mut d = self.scrollback_dirty.lock().unwrap();
        if !d.contains(surface) {
            d.insert(surface.to_string());
        }
    }

    /// Mark every terminal as changed (startup: the files may be stale).
    async fn mark_all_scrollback_dirty(&self) {
        let terms: Vec<Ref> = {
            let state = self.state.lock().await;
            state
                .all_surface_refs()
                .into_iter()
                .filter(|s| state.term(s).is_some())
                .collect()
        };
        self.scrollback_dirty.lock().unwrap().extend(terms);
    }

    /// Save the scrollback of every terminal with output since the last
    /// capture (or of every terminal when `all`), and delete files of surfaces
    /// that are gone. Returns how many files were written.
    ///
    /// A pane is only captured while tmux still tags it with the same surface:
    /// after a tmux restart pane ids are reused, and a stale id must not write
    /// one terminal's screen into another's file.
    pub async fn capture_scrollback(&self, all: bool) -> usize {
        let Ok(wins) = self.tmux.list_windows().await else {
            return 0;
        };
        let dirty: HashSet<Ref> = std::mem::take(&mut *self.scrollback_dirty.lock().unwrap());
        let in_flight = self.respawn.lock().unwrap().in_flight.clone();
        let (targets, terminals) = {
            let state = self.state.lock().await;
            let terminals: Vec<Ref> = state
                .all_surface_refs()
                .into_iter()
                .filter(|s| {
                    matches!(
                        state.surface(s).map(|x| &x.kind),
                        Some(SurfaceKind::Terminal { .. })
                    )
                })
                .collect();
            let targets: Vec<(Ref, String)> = terminals
                .iter()
                .filter(|s| (all || dirty.contains(*s)) && !in_flight.contains(*s))
                .filter_map(|s| {
                    let t = state.term(s)?;
                    wins.iter()
                        .any(|w| w.pane == t.pane && w.surface.as_deref() == Some(s.as_str()))
                        .then(|| (s.clone(), t.pane.clone()))
                })
                .collect();
            (targets, terminals)
        };
        // Anything dirty that could not be captured now is retried next time.
        {
            let done: HashSet<&Ref> = targets.iter().map(|(s, _)| s).collect();
            let mut d = self.scrollback_dirty.lock().unwrap();
            d.extend(
                dirty
                    .iter()
                    .filter(|s| !done.contains(s) && terminals.contains(s))
                    .cloned(),
            );
        }
        let mut written = 0;
        for (surface, pane) in targets {
            match self.tmux.capture_history(&pane, scrollback::CAPTURE_LINES).await {
                Ok(cap) => {
                    let body = scrollback::prepare(&cap);
                    match scrollback::save(&self.cfg.state_dir, &surface, &body).await {
                        Ok(()) => written += 1,
                        Err(e) => tracing::warn!(%surface, "saving scrollback: {e}"),
                    }
                }
                Err(e) => {
                    tracing::debug!(%surface, "capturing scrollback: {e}");
                    self.note_output(&surface);
                }
            }
        }
        scrollback::prune(&self.cfg.state_dir, &terminals).await;
        written
    }

    /// Drop a closed terminal's saved scrollback.
    pub async fn forget_scrollback(&self, surface: &str) {
        self.scrollback_dirty.lock().unwrap().remove(surface);
        scrollback::remove(&self.cfg.state_dir, surface).await;
    }

    async fn handle_window_closed(self: &Arc<Self>, window: &str) {
        // tmux also reports `%window-close` when a session grouped with ours
        // goes away (the app's per-pane view sessions for local panes), for
        // windows that are still alive in our session. Only a window that is
        // really gone closes its surface.
        if self.tmux.window_exists(window).await {
            tracing::debug!(%window, "window-close for a window that still exists; ignored");
            return;
        }
        let surface = { self.state.lock().await.remove_window(window) };
        let Some(surface) = surface else { return };
        {
            let mut state = self.state.lock().await;
            state.remove_surface(&surface);
        }
        self.forget_scrollback(&surface).await;
        tracing::info!(%surface, "surface closed by tmux");
        self.send_to_surface(
            &surface,
            Out::Msg(ServerMsg::TermExited {
                surface: surface.clone(),
            }),
        )
        .await;
        self.tree_changed();
        self.mark_dirty();
    }
}

/// Run the daemon in the foreground until Ctrl-C.
pub async fn run(cfg: Config, browser: Option<Arc<dyn BrowserBackend>>) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("creating state dir {}", cfg.state_dir.display()))?;

    let mut state = load_state(&cfg).unwrap_or_else(|| SessionState::new(cfg.host.clone()));
    state.host = cfg.host.clone();

    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let tmux = Tmux::spawn(
        TmuxConfig {
            socket: cfg.tmux_socket.clone(),
            session: "lmux".to_string(),
        },
        ev_tx,
    )
    .await
    .context("starting tmux backend")?;
    // NOTE: we deliberately do NOT use `refresh-client -f pause-after=N` for tmux
    // flow control: on tmux 3.3a it makes tmux stop sending `%output` altogether
    // (verified), which is worse than the problem. Back-pressure is handled by the
    // bounded per-client queue + backlog drop + snapshot resync instead.

    let inner = Arc::new(Inner {
        cfg: cfg.clone(),
        state: Mutex::new(state),
        tmux,
        browser,
        browser_streaming: Mutex::new(HashSet::new()),
            browser_sizes: Default::default(),
        clients: Mutex::new(Clients::default()),
        dirty: Notify::new(),
        tree_dirty: Notify::new(),
        respawn: std::sync::Mutex::new(RespawnState::default()),
        scrollback_dirty: std::sync::Mutex::new(HashSet::new()),
        prs: std::sync::Mutex::new(HashMap::new()),
        app_calls: Default::default(),
    });

    tokio::spawn(tmux_event_loop(inner.clone(), ev_rx));
    inner.reconcile().await;
    {
        let me = inner.clone();
        tokio::spawn(async move { me.reopen_browsers().await });
    }
    inner.mark_dirty();
    inner.mark_all_scrollback_dirty().await;

    // One long-lived, debounced persist task (no per-change task, no `.tmp` race).
    {
        let me = inner.clone();
        tokio::spawn(async move {
            loop {
                me.dirty.notified().await;
                loop {
                    match tokio::time::timeout(PERSIST_DEBOUNCE, me.dirty.notified()).await {
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                me.persist().await;
            }
        });
    }

    // One coalescing task sends the tree at most once per window.
    {
        let me = inner.clone();
        tokio::spawn(async move {
            let mut c = Coalescer::new(TREE_COALESCE);
            loop {
                me.tree_dirty.notified().await;
                c.arm();
                while let Some(rem) = c.remaining() {
                    if tokio::time::timeout(rem.max(Duration::from_millis(1)), me.tree_dirty.notified())
                        .await
                        .is_ok()
                    {
                        c.arm();
                    } else {
                        break;
                    }
                }
                if c.fire() {
                    let tree = me.tree().await;
                    me.broadcast(Out::Msg(ServerMsg::Tree { tree })).await;
                }
            }
        });
    }

    // cwd and agent-exit tracking for respawn after tmux is lost.
    {
        let me = inner.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(RESUME_REFRESH);
            tick.tick().await;
            loop {
                tick.tick().await;
                me.refresh_resume().await;
            }
        });
    }

    // Scrollback of changed terminals, replayed if tmux is lost.
    {
        let me = inner.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SCROLLBACK_INTERVAL);
            tick.tick().await;
            loop {
                tick.tick().await;
                me.capture_scrollback(false).await;
            }
        });
    }

    // Ports and pull requests for the cards, on their own slower timer.
    {
        let me = inner.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PROBE_REFRESH);
            loop {
                tick.tick().await;
                if me.refresh_probes().await {
                    me.tree_changed();
                }
            }
        });
    }

    // Branch labels on a background timer; tree sends never spawn `git`.
    {
        let me = inner.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(BRANCH_REFRESH);
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                if me.refresh_branches().await {
                    me.tree_changed();
                }
            }
        });
    }

    if let Some(b) = inner.browser.clone() {
        match b.subscribe_events().await {
            Ok(rx) => {
                tokio::spawn(browser_event_loop(inner.clone(), rx));
            }
            Err(e) => tracing::warn!("browser subscribe_events failed: {e}"),
        }
    }

    let unix_inner = inner.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_unix(unix_inner).await {
            tracing::error!("unix socket server stopped: {e}");
        }
    });

    let ws_inner = inner.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_ws(ws_inner).await {
            tracing::error!("websocket server stopped: {e}");
        }
    });

    tracing::info!(
        socket = %cfg.socket_path().display(),
        ws = %cfg.ws_addr,
        tmux = %cfg.tmux_socket,
        "lmuxd ready"
    );

    shutdown_signal().await;
    tracing::info!("shutting down");
    if tokio::time::timeout(SHUTDOWN_CAPTURE_TIMEOUT, inner.capture_scrollback(false))
        .await
        .is_err()
    {
        tracing::warn!("final scrollback capture timed out");
    }
    inner.persist().await;
    Ok(())
}

/// Ctrl-C, or SIGTERM (a systemd or launchd stop).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::warn!("installing SIGTERM handler: {e}"),
        }
    }
    tokio::signal::ctrl_c().await.ok();
}

fn load_state(cfg: &Config) -> Option<SessionState> {
    let path = cfg.state_path();
    let data = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<SessionState>(&data) {
        Ok(s) => {
            tracing::info!(path = %path.display(), "loaded state");
            Some(s)
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), "ignoring unreadable state: {e}");
            None
        }
    }
}

async fn tmux_event_loop(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<TmuxEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            TmuxEvent::Output { pane, data } => {
                let Some(surface) = inner.surface_for_pane(&pane).await else {
                    tracing::trace!(%pane, "output for unknown pane");
                    continue;
                };
                inner.note_output(&surface);
                // Skip the encode entirely when nobody is watching this surface.
                if !inner.has_subscribers(&surface).await {
                    tracing::trace!(%pane, %surface, "output but no subscribers");
                    continue;
                }
                if let Ok(bytes) = (Frame::TermOutput {
                    surface: &surface,
                    data: &data,
                })
                .encode()
                {
                    tracing::trace!(%pane, %surface, n = data.len(), "forwarding output");
                    inner.send_to_surface(&surface, Out::Bytes(Bytes::from(bytes))).await;
                }
            }
            TmuxEvent::WindowClosed { window } => {
                inner.handle_window_closed(&window).await;
            }
            TmuxEvent::ControlExited => {
                // The supervisor respawns the client; re-reconcile in case the
                // tmux server restarted under us.
                tracing::warn!("tmux control client exited; supervisor will respawn");
                inner.reconcile().await;
                inner.tree_changed();
            }
        }
    }
}

async fn browser_event_loop(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<BrowserEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            BrowserEvent::Frame {
                surface,
                seq,
                width,
                height,
                jpeg,
            } => {
                if !inner.has_subscribers(&surface).await {
                    continue;
                }
                if let Ok(bytes) = (Frame::BrowserFrame {
                    surface: &surface,
                    seq,
                    width,
                    height,
                    jpeg: &jpeg,
                })
                .encode()
                {
                    inner.send_to_surface(&surface, Out::Bytes(Bytes::from(bytes))).await;
                }
            }
            BrowserEvent::Meta {
                surface,
                url,
                title,
                loading,
                can_go_back,
                can_go_forward,
            } => {
                {
                    let mut state = inner.state.lock().await;
                    if let Some(s) = state.surface_mut(&surface) {
                        if let SurfaceKind::Browser {
                            url: u,
                            loading: l,
                            can_go_back: b,
                            can_go_forward: f,
                            ..
                        } = &mut s.kind
                        {
                            *u = url.clone();
                            *l = loading;
                            *b = can_go_back;
                            *f = can_go_forward;
                        }
                        if !title.is_empty() {
                            s.title = title.clone();
                        }
                    }
                }
                inner
                    .send_to_surface(
                        &surface,
                        Out::Msg(ServerMsg::BrowserMeta {
                            surface: surface.clone(),
                            url,
                            title,
                            loading,
                            can_go_back,
                            can_go_forward,
                        }),
                    )
                    .await;
                inner.tree_changed();
            }
            BrowserEvent::Closed { surface } => {
                {
                    let mut state = inner.state.lock().await;
                    state.remove_surface(&surface);
                }
                inner
                    .send_to_surface(
                        &surface,
                        Out::Msg(ServerMsg::TermExited {
                            surface: surface.clone(),
                        }),
                    )
                    .await;
                inner.tree_changed();
                inner.mark_dirty();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// unix socket
// ---------------------------------------------------------------------------

async fn serve_unix(inner: Arc<Inner>) -> anyhow::Result<()> {
    let path = inner.cfg.socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding unix socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    tracing::info!(path = %path.display(), "unix socket listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let inner = inner.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_unix_conn(inner, stream).await {
                tracing::debug!("unix conn ended: {e}");
            }
        });
    }
}

async fn handle_unix_conn(inner: Arc<Inner>, stream: tokio::net::UnixStream) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<RpcRequest>(line) {
            Ok(req) => crate::rpc::dispatch(&inner, &req).await,
            Err(e) => RpcResponse {
                id: 0,
                result: None,
                error: Some(RpcError {
                    code: RpcErrorCode::InvalidParams,
                    message: format!("bad request JSON: {e}"),
                }),
            },
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        w.write_all(out.as_bytes()).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// websocket
// ---------------------------------------------------------------------------

async fn serve_ws(inner: Arc<Inner>) -> anyhow::Result<()> {
    let addr: SocketAddr = inner
        .cfg
        .ws_addr
        .parse()
        .with_context(|| format!("bad --ws-addr {}", inner.cfg.ws_addr))?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "websocket listening");
    loop {
        let (stream, peer) = listener.accept().await?;
        let inner = inner.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_ws_conn(inner, stream, peer).await {
                tracing::debug!(%peer, "ws conn ended: {e}");
            }
        });
    }
}

async fn handle_ws_conn(
    inner: Arc<Inner>,
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();
    let outbox = ClientSink::new(CLIENT_QUEUE_MAX);
    let id = inner.register(outbox.clone()).await;
    tracing::info!(%peer, id, "ws client connected");

    outbox.push(Out::Msg(ServerMsg::Welcome {
        protocol_version: PROTOCOL_VERSION,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        host: inner.cfg.host.clone(),
    }));
    let tree = inner.tree().await;
    outbox.push(Out::Msg(ServerMsg::Tree { tree }));

    let writer_outbox = outbox.clone();
    let writer = tokio::spawn(async move {
        loop {
            let notified = writer_outbox.notify.notified();
            if writer_outbox.closed.load(Ordering::Relaxed) {
                break;
            }
            let batch = writer_outbox.take_all();
            for out in batch {
                let msg = match out {
                    Out::Msg(m) => match serde_json::to_string(&m) {
                        Ok(s) => Message::Text(s.into()),
                        Err(_) => continue,
                    },
                    Out::Bytes(b) => Message::Binary(b.to_vec()),
                };
                if sink.send(msg).await.is_err() {
                    writer_outbox.close();
                    return;
                }
            }
            notified.await;
        }
    });

    while let Some(msg) = source.next().await {
        match msg {
            Ok(Message::Text(t)) => match serde_json::from_str::<ClientMsg>(&t) {
                Ok(cm) => inner.handle_client_msg(id, cm, &outbox).await,
                Err(e) => tracing::debug!("bad client msg: {e}"),
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("ws read error: {e}");
                break;
            }
        }
    }

    outbox.close();
    inner.unregister(id).await;
    writer.abort();
    tracing::info!(%peer, id, "ws client disconnected");
    Ok(())
}

impl Inner {
    async fn handle_client_msg(
        self: &Arc<Self>,
        id: u64,
        msg: ClientMsg,
        outbox: &Arc<ClientSink>,
    ) {
        match msg {
            ClientMsg::Hello { client_name, .. } => {
                if client_name.starts_with("lmux-app") {
                    self.clients.lock().await.apps.push(id);
                }
            }
            ClientMsg::AppBrowserResult { id: call, value, error } => {
                self.app_calls.lock().unwrap().finish(call, value, error);
            }
            ClientMsg::Rpc { request } => {
                let resp = crate::rpc::dispatch(self, &request).await;
                outbox.push(Out::Msg(ServerMsg::RpcResult { response: resp }));
            }
            ClientMsg::Subscribe { surface, history } => {
                let count = self.add_subscription(id, &surface).await;
                let kind = self
                    .state
                    .lock()
                    .await
                    .surface(&surface)
                    .map(|s| s.kind.clone());
                match kind {
                    Some(SurfaceKind::Terminal { .. }) => {
                        if let Some(term) = self.surface_term(&surface).await {
                            match self.tmux.snapshot_with_history(&term.pane, history.unwrap_or(0)).await {
                                Ok((cols, rows, data)) => {
                                    outbox.push(Out::Msg(ServerMsg::TermSnapshot {
                                        surface,
                                        cols,
                                        rows,
                                        data,
                                    }));
                                }
                                Err(e) => tracing::warn!("snapshot failed: {e}"),
                            }
                        }
                    }
                    Some(SurfaceKind::Browser { .. }) => {
                        if count == 1 {
                            // A sole viewer means nobody is drawing frames.
                            self.clear_streaming(&surface).await;
                            self.maybe_start_fallback_screencast(surface.clone());
                        }
                    }
                    None => {}
                }
            }
            ClientMsg::Unsubscribe { surface } => {
                let count = self.remove_subscription(id, &surface).await;
                let is_browser = matches!(
                    self.state.lock().await.surface(&surface).map(|s| s.kind.clone()),
                    Some(SurfaceKind::Browser { .. })
                );
                if is_browser && count == 0 {
                    self.clear_streaming(&surface).await;
                    if let Some(b) = &self.browser {
                        let _ = b.stop_screencast(&surface).await;
                    }
                }
            }
            ClientMsg::TermInput { surface, data } => {
                if let Some(term) = self.surface_term(&surface).await {
                    if let Err(e) = self.tmux.send_bytes(&term.pane, data.as_bytes()).await {
                        tracing::warn!("term input failed: {e}");
                    }
                }
            }
            ClientMsg::TermResize {
                surface,
                cols,
                rows,
            } => {
                if let Some(term) = self.surface_term(&surface).await {
                    let _ = self.tmux.resize(&term.window, cols, rows).await;
                    self.state
                        .lock()
                        .await
                        .set_pane_surface_size(&surface, cols, rows);
                    // Sizes are part of the tree; coalesced, so a drag is one send.
                    self.tree_changed();
                }
            }
            ClientMsg::BrowserResize {
                surface,
                width,
                height,
                device_scale_factor,
            } => {
                // The client's own size drives the screencast; we never invent a
                // 2x scale. Starting here also means a hidden pane stays dark.
                self.browser_sizes
                    .lock()
                    .unwrap()
                    .insert(surface.clone(), (width, height, device_scale_factor));
                if let Some(b) = &self.browser {
                    if b
                        .start_screencast(&surface, width, height, device_scale_factor)
                        .await
                        .is_ok()
                    {
                        self.mark_streaming(&surface).await;
                    }
                }
            }
            ClientMsg::BrowserInput { surface, event } => {
                if let Some(b) = &self.browser {
                    if let Err(e) = b.input(&surface, event).await {
                        tracing::warn!("browser input failed: {e}");
                    }
                }
            }
            ClientMsg::BrowserFrameAck { surface, seq } => {
                if let Some(b) = &self.browser {
                    let _ = b.ack_frame(&surface, seq).await;
                }
            }
        }
    }

    /// If a client subscribes to a browser surface but never sends a resize, start
    /// a stream after a short grace period: at the size it was last shown at,
    /// else a sane non-Retina one.
    fn maybe_start_fallback_screencast(self: &Arc<Self>, surface: Ref) {
        let Some(b) = self.browser.clone() else {
            return;
        };
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            if me.subscriber_count(&surface).await == 0 || me.is_streaming(&surface).await {
                return;
            }
            let (w, h, dsf) = me
                .browser_sizes
                .lock()
                .unwrap()
                .get(&surface)
                .copied()
                .unwrap_or((1280, 800, 1.0));
            if b
                .start_screencast(&surface, w, h, dsf)
                .await
                .is_ok()
            {
                me.mark_streaming(&surface).await;
            }
        });
    }
}

/// Build a params object with the caller defaults injected (used by tests).
pub fn with_caller(
    mut params: serde_json::Value,
    workspace: Option<&str>,
    surface: Option<&str>,
) -> serde_json::Value {
    if let Some(obj) = params.as_object_mut() {
        if let Some(w) = workspace {
            obj.insert("caller_workspace".into(), json!(w));
        }
        if let Some(s) = surface {
            obj.insert("caller_surface".into(), json!(s));
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_for_term_output() {
        let f = Frame::TermOutput {
            surface: "surface:3",
            data: b"\x1b[1mhi\r\n",
        };
        let bytes = f.encode().unwrap();
        assert_eq!(Frame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn caller_params_injected() {
        let v = with_caller(json!({}), Some("workspace:2"), Some("surface:3"));
        assert_eq!(v["caller_workspace"], "workspace:2");
        assert_eq!(v["caller_surface"], "surface:3");
    }

    #[test]
    fn agent_tags_and_ids() {
        let a = parse_agent_tag("claude:0b6f7c1e-1a2b-4c3d-9e8f-001122334455").unwrap();
        assert_eq!(a.agent, "claude");
        assert_eq!(
            resume_command(&a).unwrap(),
            "claude --resume 0b6f7c1e-1a2b-4c3d-9e8f-001122334455\r"
        );
        assert!(parse_agent_tag("").is_none());
        let c = parse_agent_tag("codex:019f4dbf-d058-78d0-a3cc-1c0f28477ca6").unwrap();
        assert_eq!(resume_command(&c).unwrap(), "codex resume 019f4dbf-d058-78d0-a3cc-1c0f28477ca6\r");
        assert!(parse_agent_tag("vim:abc").is_none(), "only agents lmux knows how to resume");
        assert!(parse_agent_tag("claude:").is_none());
        assert!(parse_agent_tag("claude:abc; rm -rf ~").is_none());
        assert!(parse_agent_tag("claude:abc\ndef").is_none());
        assert!(!crate::tree::valid_agent_session_id(&"a".repeat(129)));
    }

    #[test]
    fn shell_detection() {
        for sh in ["zsh", "-zsh", "bash", "fish", "sh"] {
            assert!(is_shell(sh), "{sh}");
        }
        for cmd in ["claude", "node", "vim", "python3"] {
            assert!(!is_shell(cmd), "{cmd}");
        }
    }

    #[test]
    fn old_state_without_resume_loads() {
        let json = r#"{"host":"h","workspaces":[],"selected_workspace":null}"#;
        let s: SessionState = serde_json::from_str(json).unwrap();
        assert!(s.resume.is_empty());
        let mut s = s;
        s.resume_mut("surface:9").cwd = Some("/tmp".into());
        s.prune_resume();
        assert!(s.resume.is_empty(), "entries for unknown surfaces are pruned");
    }

    #[test]
    fn branch_parsing() {
        assert_eq!(parse_branch_output("main\n"), Some("main".to_string()));
        assert_eq!(
            parse_branch_output("feature/x  \n"),
            Some("feature/x".to_string())
        );
        // Detached HEAD and empty output both fall through to the short-commit path.
        assert_eq!(parse_branch_output("HEAD\n"), None);
        assert_eq!(parse_branch_output(""), None);
        assert_eq!(parse_branch_output("   \n"), None);
    }

    #[tokio::test]
    async fn git_branch_none_for_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_branch(dir.path().to_str().unwrap()).await, None);
        // A cwd that does not exist must not panic either.
        assert_eq!(git_branch("/nonexistent/lmux/xyz").await, None);
    }

    #[test]
    fn config_paths() {
        let mut cfg = Config {
            state_dir: PathBuf::from("/tmp/lmux-test"),
            socket: None,
            tmux_socket: "s".into(),
            ws_addr: "127.0.0.1:1".into(),
            host: "h".into(),
            app_browser: false,
        };
        assert_eq!(cfg.socket_path(), PathBuf::from("/tmp/lmux-test/lmux.sock"));
        // An explicit socket wins over the state directory.
        cfg.socket = Some(PathBuf::from("/tmp/s.sock"));
        assert_eq!(cfg.socket_path(), PathBuf::from("/tmp/s.sock"));
        assert_eq!(cfg.state_path(), PathBuf::from("/tmp/lmux-test/state.json"));
    }

    #[test]
    fn client_sink_is_bounded_and_clearable() {
        let sink = ClientSink::new(3);
        let msg = |n: u32| Out::Msg(ServerMsg::TermSnapshot {
            surface: "s".into(),
            cols: n as u16,
            rows: 1,
            data: String::new(),
        });
        assert!(sink.push(msg(1)));
        assert!(sink.push(msg(2)));
        assert!(sink.push(msg(3)));
        // Fourth push overflows: the producer must recover, not buffer.
        assert!(!sink.push(msg(4)));
        assert_eq!(sink.len(), 3);
        // Re-arming after a clear works.
        sink.clear();
        assert_eq!(sink.len(), 0);
        assert!(sink.push(msg(5)));
        assert_eq!(sink.take_all().len(), 1);
        assert_eq!(sink.len(), 0);
    }

    #[test]
    fn backlog_drop_keeps_only_latest_state() {
        // Simulate the recovery path: fill, overflow, clear, then a fresh snapshot.
        let sink = ClientSink::new(2);
        for _ in 0..2 {
            sink.push(Out::Bytes(Bytes::from_static(b"chunk")));
        }
        assert!(!sink.push(Out::Bytes(Bytes::from_static(b"chunk"))));
        sink.clear();
        sink.push(Out::Msg(ServerMsg::TermSnapshot {
            surface: "s".into(),
            cols: 80,
            rows: 24,
            data: "screen".into(),
        }));
        let drained = sink.take_all();
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            Out::Msg(ServerMsg::TermSnapshot { data, .. }) => assert_eq!(data, "screen"),
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    async fn tmux_cmd(socket: &str, args: &[&str]) -> bool {
        tokio::process::Command::new("tmux")
            .args(["-L", socket])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Poll `f` until it returns true or `secs` pass.
    async fn wait_for<F, Fut>(secs: u64, mut f: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if f().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        false
    }

    /// With `--app-browser` there is no Chromium: a browser pane is made for
    /// the app to draw, and the app reports its page back.
    #[tokio::test]
    async fn app_browser_panes_are_drawn_by_the_app() {
        let socket = format!("lmux-test-ab-{}", std::process::id());
        let state_dir = tempfile::tempdir().unwrap();
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let tmux = match Tmux::spawn(TmuxConfig { socket: socket.clone(), session: "lmux".into() }, ev_tx).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping: could not start tmux ({e})");
                return;
            }
        };
        let mut state = SessionState::new("t");
        let ws = state.next_workspace();
        let pane = state.next_pane();
        state.add_workspace(Workspace {
            id: ws,
            title: "w".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: pane.clone() },
            panes: vec![Pane { id: pane.clone(), surfaces: vec![], active_surface: None }],
            focused_pane: Some(pane.clone()),
            ports: vec![],
            pr: None,
        });
        let inner = Arc::new(Inner {
            cfg: Config {
                state_dir: state_dir.path().to_path_buf(),
                socket: None,
                tmux_socket: socket.clone(),
                ws_addr: "127.0.0.1:1".into(),
                host: "t".into(),
                app_browser: true,
            },
            state: Mutex::new(state),
            tmux,
            browser: None,
            browser_streaming: Mutex::new(HashSet::new()),
            browser_sizes: Default::default(),
            clients: Mutex::new(Clients::default()),
            dirty: Notify::new(),
            tree_dirty: Notify::new(),
            respawn: std::sync::Mutex::new(RespawnState::default()),
            scrollback_dirty: std::sync::Mutex::new(HashSet::new()),
            prs: std::sync::Mutex::new(HashMap::new()),
            app_calls: Default::default(),
        });
        let call = |method: &str, params: serde_json::Value| RpcRequest { id: 1, method: method.into(), params };

        // A tab saved while this host ran Chromium is the app's to draw now.
        let old = inner.state.lock().await.next_surface();
        inner.state.lock().await.add_surface(&pane, Surface {
            id: old.clone(),
            title: "old".into(),
            kind: SurfaceKind::Browser { url: "https://a.test".into(), loading: false, can_go_back: false, can_go_forward: false, in_app: false },
        });
        inner.reopen_browsers().await;
        assert!(matches!(
            inner.state.lock().await.surface(&old).map(|s| s.kind.clone()),
            Some(SurfaceKind::Browser { in_app: true, .. })
        ));

        let r = crate::rpc::dispatch(&inner, &call(methods::SURFACE_NEW, json!({"pane": pane, "type": "browser", "url": "https://example.com"}))).await;
        assert!(r.error.is_none(), "{:?}", r.error);
        let surf = r.result.unwrap()["surface"].as_str().unwrap().to_owned();
        async fn kind(inner: &Inner, surf: &str) -> Option<(String, SurfaceKind)> {
            inner.state.lock().await.surface(surf).map(|s| (s.title.clone(), s.kind.clone()))
        }
        assert!(matches!(kind(&inner, &surf).await, Some((_, SurfaceKind::Browser { in_app: true, .. }))));

        let r = crate::rpc::dispatch(&inner, &call(methods::BROWSER_REPORT, json!({"surface": surf, "url": "https://example.com/x", "title": "Example", "can_go_back": true}))).await;
        assert!(r.error.is_none(), "{:?}", r.error);
        match kind(&inner, &surf).await {
            Some((title, SurfaceKind::Browser { url, can_go_back, .. })) => {
                assert_eq!((title.as_str(), url.as_str(), can_go_back), ("Example", "https://example.com/x", true));
            }
            other => panic!("{other:?}"),
        }

        let r = crate::rpc::dispatch(&inner, &call(methods::BROWSER_SNAPSHOT, json!({"surface": surf}))).await;
        assert_eq!(r.error.map(|e| e.code), Some(RpcErrorCode::Unsupported));
        tmux_cmd(&socket, &["kill-server"]).await;
    }

    /// A viewer that drops mid-stream (its last frame never acked) must not
    /// leave the browser marked as streaming: the next viewer, which subscribes
    /// without a resize, gets a stream at the size the pane was shown at.
    #[tokio::test]
    async fn browser_reopens_and_streams_after_restart_and_disconnect() {
        let socket = format!("lmux-test-bs-{}", std::process::id());
        let state_dir = tempfile::tempdir().unwrap();
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let tmux = match Tmux::spawn(TmuxConfig { socket: socket.clone(), session: "lmux".into() }, ev_tx).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping: could not start tmux ({e})");
                return;
            }
        };
        let mut state = SessionState::new("t");
        let ws = state.next_workspace();
        let pane = state.next_pane();
        let surf = state.next_surface();
        state.add_workspace(Workspace {
            id: ws,
            title: "w".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: pane.clone() },
            panes: vec![Pane {
                id: pane.clone(),
                surfaces: vec![Surface {
                    id: surf.clone(),
                    title: "b".into(),
                    kind: SurfaceKind::Browser {
                        url: "about:blank".into(),
                        loading: false,
                        can_go_back: false,
                        can_go_forward: false,
                        in_app: false,
                    },
                }],
                active_surface: Some(surf.clone()),
            }],
            focused_pane: Some(pane),
            ports: vec![],
            pr: None,
        });
        let fake = Arc::new(crate::browser::FakeBrowser::new());
        let inner = Arc::new(Inner {
            cfg: Config {
                state_dir: state_dir.path().to_path_buf(),
                socket: None,
                tmux_socket: socket.clone(),
                ws_addr: "127.0.0.1:1".into(),
                host: "t".into(),
                app_browser: false,
            },
            state: Mutex::new(state),
            tmux,
            browser: Some(fake.clone()),
            browser_streaming: Mutex::new(HashSet::new()),
            browser_sizes: Default::default(),
            clients: Mutex::new(Clients::default()),
            dirty: Notify::new(),
            tree_dirty: Notify::new(),
            respawn: std::sync::Mutex::new(RespawnState::default()),
            scrollback_dirty: std::sync::Mutex::new(HashSet::new()),
            prs: std::sync::Mutex::new(HashMap::new()),
            app_calls: Default::default(),
        });

        // A restart reopens the saved browser tab at its URL.
        inner.reopen_browsers().await;
        assert_eq!(fake.calls.lock().unwrap().clone(), vec![format!("create {surf} Some(\"about:blank\")")]);

        let a_sink = ClientSink::new(64);
        let a = inner.register(a_sink.clone()).await;
        inner.handle_client_msg(a, ClientMsg::Subscribe { surface: surf.clone(), history: None }, &a_sink).await;
        let resize = ClientMsg::BrowserResize { surface: surf.clone(), width: 900, height: 600, device_scale_factor: 1.0 };
        inner.handle_client_msg(a, resize, &a_sink).await;
        inner.unregister(a).await;
        assert!(!inner.is_streaming(&surf).await);
        assert!(fake.calls.lock().unwrap().contains(&format!("stop {surf}")));

        let b_sink = ClientSink::new(64);
        let b = inner.register(b_sink.clone()).await;
        inner.handle_client_msg(b, ClientMsg::Subscribe { surface: surf.clone(), history: None }, &b_sink).await;
        let started = wait_for(5, || async { inner.is_streaming(&surf).await }).await;
        tmux_cmd(&socket, &["kill-server"]).await;
        assert!(started, "no stream for the second viewer: {:?}", fake.calls.lock().unwrap());
        assert_eq!(fake.calls.lock().unwrap().last().unwrap(), &format!("start {surf} 900x600"));
    }

    /// End to end: output in a terminal is saved to its scrollback file; after
    /// the tmux server is killed, reconcile respawns the terminal in its old
    /// directory and the new pane shows the saved output above a fresh shell.
    #[tokio::test]
    async fn scrollback_is_saved_and_replayed_after_tmux_is_lost() {
        if !tmux_cmd("lmux-test-probe-unused", &["-V"]).await {
            eprintln!("skipping: tmux not available");
            return;
        }
        let socket = format!("lmux-test-sb-{}", std::process::id());
        let state_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(work.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let tmux = match Tmux::spawn(
            TmuxConfig {
                socket: socket.clone(),
                session: "lmux".into(),
            },
            ev_tx,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping: could not start tmux ({e})");
                return;
            }
        };
        // A plain shell keeps the test independent of the user's rc files.
        tmux_cmd(&socket, &["set-option", "-g", "default-shell", "/bin/sh"]).await;

        let mut state = SessionState::new("t");
        let ws = state.next_workspace();
        let pane_ref = state.next_pane();
        let surf = state.next_surface();
        state.add_workspace(Workspace {
            id: ws.clone(),
            title: "w".into(),
            folder: None,
            cwd: cwd.clone(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane {
                pane: pane_ref.clone(),
            },
            panes: vec![Pane {
                id: pane_ref.clone(),
                surfaces: vec![terminal_surface(surf.clone(), "t", 80, 24)],
                active_surface: Some(surf.clone()),
            }],
            focused_pane: Some(pane_ref),
            ports: vec![],
            pr: None,
        });
        let inner = Arc::new(Inner {
            cfg: Config {
                state_dir: state_dir.path().to_path_buf(),
                socket: None,
                tmux_socket: socket.clone(),
                ws_addr: "127.0.0.1:1".into(),
                host: "t".into(),
                app_browser: false,
            },
            state: Mutex::new(state),
            tmux,
            browser: None,
            browser_streaming: Mutex::new(HashSet::new()),
            browser_sizes: Default::default(),
            clients: Mutex::new(Clients::default()),
            dirty: Notify::new(),
            tree_dirty: Notify::new(),
            respawn: std::sync::Mutex::new(RespawnState::default()),
            scrollback_dirty: std::sync::Mutex::new(HashSet::new()),
            prs: std::sync::Mutex::new(HashMap::new()),
            app_calls: Default::default(),
        });

        let result = async {
            let (_w, pane) =
                crate::rpc::spawn_terminal(&inner, &ws, &surf, "t", &cwd, None, 80, 24)
                    .await
                    .map_err(|e| format!("spawn: {e}"))?;
            // The typed line holds `$((6*7))`; only the output holds `42`.
            inner
                .tmux
                .send_bytes(&pane, b"printf 'SB_MARKER_%s\\n' $((6*7))\r")
                .await
                .map_err(|e| format!("send: {e}"))?;
            let shown = wait_for(10, || async {
                inner
                    .tmux
                    .capture(&pane, false, None)
                    .await
                    .is_ok_and(|c| c.contains("SB_MARKER_42"))
            })
            .await;
            if !shown {
                return Err("marker never appeared".to_string());
            }

            // Capture: only dirty terminals unless `all`.
            if inner.capture_scrollback(false).await != 0 {
                return Err("nothing was dirty, nothing should be written".into());
            }
            inner.note_output(&surf);
            if inner.capture_scrollback(false).await != 1 {
                return Err("dirty terminal was not captured".into());
            }
            let file = scrollback::path(state_dir.path(), &surf);
            let saved = std::fs::read_to_string(&file).map_err(|e| format!("read: {e}"))?;
            if !saved.contains("SB_MARKER_42") {
                return Err(format!("saved scrollback lacks the marker: {saved:?}"));
            }

            // Lose tmux. The supervisor brings a fresh server back.
            tmux_cmd(&socket, &["kill-server"]).await;
            let back = wait_for(15, || async {
                tmux_cmd(&socket, &["has-session", "-t", "lmux"]).await
                    && inner.tmux.list_windows().await.is_ok()
            })
            .await;
            if !back {
                return Err("tmux did not come back".into());
            }
            tmux_cmd(&socket, &["set-option", "-g", "default-shell", "/bin/sh"]).await;

            inner.reconcile().await;
            let mut new_pane = String::new();
            let mut replayed = false;
            let deadline = Instant::now() + Duration::from_secs(15);
            while !replayed && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let Ok(wins) = inner.tmux.list_windows().await else {
                    continue;
                };
                let Some(w) = wins.iter().find(|w| w.surface.as_deref() == Some(surf.as_str()))
                else {
                    continue;
                };
                new_pane = w.pane.clone();
                replayed = inner
                    .tmux
                    .capture(&w.pane, false, Some(-2000))
                    .await
                    .is_ok_and(|c| {
                        c.contains("SB_MARKER_42") && c.contains(scrollback::RESTORE_BANNER)
                    });
            }
            if !replayed {
                let screen = inner.tmux.capture(&new_pane, false, Some(-2000)).await;
                return Err(format!("respawned pane did not replay scrollback: {screen:?}"));
            }
            let pane_cwd = inner
                .tmux
                .display(&new_pane, "#{pane_current_path}")
                .await
                .map_err(|e| format!("display: {e}"))?;
            if pane_cwd.trim() != cwd {
                return Err(format!("respawned in {pane_cwd:?}, expected {cwd:?}"));
            }
            // The one-shot copy is consumed; the saved file stays until close.
            let consumed = wait_for(5, || async {
                !scrollback::restore_path(state_dir.path(), &surf).exists()
            })
            .await;
            if !consumed {
                return Err("restore copy was not removed".into());
            }
            inner.forget_scrollback(&surf).await;
            if file.exists() {
                return Err("closing the surface must delete its scrollback".into());
            }
            Ok(())
        }
        .await;

        tmux_cmd(&socket, &["kill-server"]).await;
        if let Err(e) = result {
            panic!("{e}");
        }
    }

    #[test]
    fn coalescer_fires_once_per_window() {
        let mut c = Coalescer::new(Duration::from_millis(30));
        assert!(c.remaining().is_none());
        c.arm();
        assert!(c.remaining().is_some());
        // A second arm does not extend the window.
        c.arm();
        assert!(!c.fire(), "must not fire before the window elapses");
        std::thread::sleep(Duration::from_millis(40));
        assert!(c.fire(), "fires after the window");
        assert!(!c.fire(), "only once");
        // Re-usable after firing.
        c.arm();
        assert!(c.remaining().is_some());
        std::thread::sleep(Duration::from_millis(40));
        assert!(c.fire());
    }
}
