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
use crate::tmux::{Tmux, TmuxConfig, TmuxEvent};
use crate::tree::{terminal_surface, SessionState, TermMeta};

#[derive(Debug, Clone)]
pub struct Config {
    pub state_dir: PathBuf,
    pub tmux_socket: String,
    pub ws_addr: String,
    pub host: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            state_dir: crate::default_state_dir(),
            tmux_socket: "lmux".to_string(),
            ws_addr: crate::default_ws_addr(),
            host: hostname(),
        }
    }
}

impl Config {
    pub fn socket_path(&self) -> PathBuf {
        self.state_dir.join("lmux.sock")
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

/// Persist debounce: coalesce a burst of changes into one write.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);

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
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    if let Some(b) = parse_branch_output(&String::from_utf8_lossy(&out.stdout)) {
        return Some(b);
    }
    // Detached HEAD: abbrev-ref printed "HEAD"; show the short commit instead.
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_branch_output(&String::from_utf8_lossy(&out.stdout))
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
    clients: Mutex<Clients>,
    /// Wakes the persist task.
    dirty: Notify,
    /// Wakes the tree-coalescing task.
    tree_dirty: Notify,
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

    async fn unregister(&self, id: u64) {
        let mut c = self.clients.lock().await;
        c.conns.remove(&id);
        for set in c.subs.values_mut() {
            set.remove(&id);
        }
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
        {
            let mut state = self.state.lock().await;

            // 1. Drop surfaces whose tmux pane is gone; refresh live ones.
            let surfaces = state.all_surface_refs();
            for surf in surfaces {
                let kind = state.surface(&surf).map(|s| s.kind.clone());
                match kind {
                    Some(SurfaceKind::Terminal { tmux_pane, .. }) => match tmux_pane {
                        Some(p) if live.contains(&p) => {
                            if let Some(w) = wins.iter().find(|w| w.pane == p) {
                                state.set_pane_surface_size(&surf, w.cols, w.rows);
                                state.set_term(&surf, &w.window, &p);
                            }
                        }
                        _ => {
                            state.remove_surface(&surf);
                        }
                    },
                    Some(SurfaceKind::Browser { .. }) => {
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
            }

            if state.selected_workspace.is_none() {
                state.selected_workspace = state.workspaces.first().map(|w| w.id.clone());
            }
        }
        tracing::info!(windows = wins.len(), "reconciled with tmux");
    }

    async fn handle_window_closed(self: &Arc<Self>, window: &str) {
        let surface = { self.state.lock().await.remove_window(window) };
        let Some(surface) = surface else { return };
        {
            let mut state = self.state.lock().await;
            state.remove_surface(&surface);
        }
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
        clients: Mutex::new(Clients::default()),
        dirty: Notify::new(),
        tree_dirty: Notify::new(),
    });

    tokio::spawn(tmux_event_loop(inner.clone(), ev_rx));
    inner.reconcile().await;
    inner.mark_dirty();

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

    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutting down");
    inner.persist().await;
    Ok(())
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
            ClientMsg::Hello { .. } => {}
            ClientMsg::Rpc { request } => {
                let resp = crate::rpc::dispatch(self, &request).await;
                outbox.push(Out::Msg(ServerMsg::RpcResult { response: resp }));
            }
            ClientMsg::Subscribe { surface } => {
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
                            match self.tmux.snapshot(&term.pane).await {
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
                if let Some(b) = &self.browser {
                    let _ = b
                        .start_screencast(&surface, width, height, device_scale_factor)
                        .await;
                    self.mark_streaming(&surface).await;
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
    /// a sane non-Retina stream after a short grace period.
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
            if b
                .start_screencast(&surface, 1280, 800, 1.0)
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
        let cfg = Config {
            state_dir: PathBuf::from("/tmp/lmux-test"),
            tmux_socket: "s".into(),
            ws_addr: "127.0.0.1:1".into(),
            host: "h".into(),
        };
        assert_eq!(cfg.socket_path(), PathBuf::from("/tmp/lmux-test/lmux.sock"));
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
