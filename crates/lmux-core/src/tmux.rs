//! tmux control-mode backend.
//!
//! One private tmux server (`tmux -L <socket>`), one session (`default: lmux`),
//! one window per terminal surface. A single long-lived control client
//! (`tmux -C attach`) both runs commands and streams `%output` for every pane in
//! the session (verified on tmux 3.3a: output arrives for non-active windows too).
//!
//! The control stream is read as **raw bytes** (`read_until(b'\n')`): tmux passes
//! non-UTF-8 output through `%output`, and a UTF-8-decoding read used to kill the
//! client on the first high byte. A supervisor task also respawns the control
//! client if it ever exits, so a broken stream can never be permanent.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::keys::{map_key_name, unescape_output_bytes};

#[derive(Debug, Clone)]
pub struct TmuxConfig {
    pub socket: String,
    pub session: String,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        TmuxConfig {
            socket: "lmux".to_string(),
            session: "lmux".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TmuxEvent {
    /// Raw output for a pane (`%<n>`), already unescaped.
    Output { pane: String, data: Vec<u8> },
    /// A window was closed (`%window-close` / `%unlinked-window-close`).
    WindowClosed { window: String },
    /// The control client exited; the supervisor is respawning it. The daemon
    /// should re-reconcile (the tmux server may have restarted).
    ControlExited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinInfo {
    pub window: String,
    pub pane: String,
    pub surface: Option<String>,
    pub workspace: Option<String>,
    pub title: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// `pane_current_path`: where the pane's foreground process is.
    pub cwd: Option<String>,
    /// `pane_current_command`: the foreground process name (`zsh`, `claude`…).
    pub command: Option<String>,
    /// `@lmux_agent_session`, `<agent>:<id>`; mirrors the daemon's resume map so
    /// a lost `state.json` does not lose it.
    pub agent_session: Option<String>,
}

struct TmuxCmd {
    line: String,
    reply: oneshot::Sender<anyhow::Result<String>>,
}

struct Shared {
    socket: String,
    session: String,
    cmd_tx: Mutex<Option<mpsc::Sender<TmuxCmd>>>,
    ready: watch::Sender<bool>,
    events: mpsc::UnboundedSender<TmuxEvent>,
}

impl Shared {
    async fn install(&self, tx: Option<mpsc::Sender<TmuxCmd>>) {
        let ready = tx.is_some();
        *self.cmd_tx.lock().await = tx;
        let _ = self.ready.send(ready);
    }
}

#[derive(Clone)]
pub struct Tmux {
    shared: Arc<Shared>,
    session: String,
}

impl Tmux {
    /// Ensure the session exists, attach a control client and wait until it is
    /// ready to accept commands.
    pub async fn spawn(
        cfg: TmuxConfig,
        events: mpsc::UnboundedSender<TmuxEvent>,
    ) -> anyhow::Result<Tmux> {
        ensure_session(&cfg).await?;

        let (ready_tx, _ready_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            socket: cfg.socket.clone(),
            session: cfg.session.clone(),
            cmd_tx: Mutex::new(None),
            ready: ready_tx,
            events,
        });

        let (first_tx, first_rx) = oneshot::channel::<anyhow::Result<()>>();
        tokio::spawn(supervisor(shared.clone(), first_tx));

        match tokio::time::timeout(Duration::from_secs(10), first_rx).await {
            Ok(Ok(Ok(()))) => Ok(Tmux {
                shared,
                session: cfg.session,
            }),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(anyhow!("tmux supervisor dropped before becoming ready")),
            Err(_) => Err(anyhow!("timed out waiting for tmux control client")),
        }
    }

    async fn command(&self, line: impl Into<String>) -> anyhow::Result<String> {
        let line = line.into();
        let mut ready = self.shared.ready.subscribe();
        loop {
            let tx = self.shared.cmd_tx.lock().await.clone();
            if let Some(tx) = tx {
                let (reply, rx) = oneshot::channel();
                if tx
                    .send(TmuxCmd {
                        line: line.clone(),
                        reply,
                    })
                    .await
                    .is_ok()
                {
                    return rx
                        .await
                        .map_err(|_| anyhow!("tmux command reply dropped"))?;
                }
                // The sender just died; fall through and wait for a respawn.
            }
            match tokio::time::timeout(Duration::from_secs(30), ready.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(anyhow!("tmux supervisor stopped")),
                Err(_) => return Err(anyhow!("timed out waiting for tmux control client")),
            }
        }
    }

    pub fn is_alive(&self) -> bool {
        *self.shared.ready.borrow()
    }

    /// Create a window for a terminal surface. Returns `(window_id, pane_id)`.
    ///
    /// All setup is one chained tmux command (`;` separated): the new window
    /// becomes the session's current window, so the following `-t <session>`
    /// options target it. This turns six tmux round trips into one.
    #[allow(clippy::too_many_arguments)]
    pub async fn new_terminal(
        &self,
        cwd: &str,
        command: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        title: &str,
        surface: &str,
        workspace: &str,
    ) -> anyhow::Result<(String, String)> {
        // `name:` always means the session (and, for options, its current
        // window). A bare `name` is also matched against window names, so a
        // workspace titled like the session made new-window pick that window's
        // index ("index 1 in use") and the options land on the wrong window.
        let session = shell_quote(&format!("{}:", self.session));
        let mut line = format!(
            "new-window -t {session} -P -F \"#{{window_id}} #{{pane_id}}\" -n {} -c {}",
            shell_quote(&sanitize_window_name(title)),
            shell_quote(cwd),
        );
        for (k, v) in env {
            line.push_str(&format!(" -e {}", shell_quote(&format!("{k}={v}"))));
        }
        if let Some(cmd) = command {
            line.push(' ');
            line.push_str(&shell_quote(cmd));
        }
        // per-window manual size (global stays `latest` to avoid a tmux 3.3a
        // crash when new-window runs with global window-size manual), then size,
        // then the tags used for reconciliation.
        line.push_str(&format!(
            " ; set-option -w -t {session} window-size manual"
        ));
        line.push_str(&format!(
            " ; resize-window -t {session} -x {} -y {}",
            cols.max(2),
            rows.max(2)
        ));
        line.push_str(&format!(
            " ; set-option -w -t {session} @lmux_surface {}",
            shell_quote(surface)
        ));
        line.push_str(&format!(
            " ; set-option -w -t {session} @lmux_workspace {}",
            shell_quote(workspace)
        ));
        line.push_str(&format!(
            " ; set-option -w -t {session} @lmux_title {}",
            shell_quote(title)
        ));

        let out = self.command(line).await?;
        let mut it = out.split_whitespace();
        let window = it
            .next()
            .ok_or_else(|| anyhow!("new-window: no window id"))?
            .to_string();
        let pane = it
            .next()
            .ok_or_else(|| anyhow!("new-window: no pane id"))?
            .to_string();
        Ok((window, pane))
    }

    pub async fn resize(&self, window: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        let cols = cols.max(2);
        let rows = rows.max(2);
        self.command(format!("resize-window -t {window} -x {cols} -y {rows}"))
            .await?;
        Ok(())
    }

    pub async fn set_window_option(
        &self,
        window: &str,
        name: &str,
        value: &str,
    ) -> anyhow::Result<()> {
        // `-w` is essential: without it tmux sets a session option, which leaks
        // to every window (and setting window-size at session scope makes the
        // next new-window crash on tmux 3.3a).
        self.command(format!(
            "set-option -w -t {} {} {}",
            window,
            name,
            shell_quote(value)
        ))
        .await?;
        Ok(())
    }

    pub async fn send_bytes(&self, pane: &str, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.command(format!("send-keys -t {} -H {}", pane, hex.join(" ")))
            .await?;
        Ok(())
    }

    /// Send a tmux key name (already mapped).
    pub async fn send_key(&self, pane: &str, key: &str) -> anyhow::Result<()> {
        self.command(format!("send-keys -t {} {}", pane, shell_quote(key)))
            .await?;
        Ok(())
    }

    /// Send a human key name, mapping it first.
    pub async fn send_named_key(&self, pane: &str, key: &str) -> anyhow::Result<()> {
        let mapped = map_key_name(key).ok_or_else(|| anyhow!("unknown key name: {key}"))?;
        self.send_key(pane, &mapped).await
    }

    pub async fn capture(
        &self,
        pane: &str,
        escape: bool,
        start_line: Option<i32>,
    ) -> anyhow::Result<String> {
        let mut line = format!("capture-pane -p -t {pane}");
        if escape {
            line.push_str(" -e");
        }
        if let Some(n) = start_line {
            line.push_str(&format!(" -S {}", n));
        }
        self.command(line).await
    }

    /// Human-facing plain screen capture.
    pub async fn read_screen(
        &self,
        pane: &str,
        scrollback: bool,
        lines: Option<u32>,
    ) -> anyhow::Result<String> {
        if scrollback {
            let start = lines.map(|n| -(n as i32)).unwrap_or(-2000);
            self.capture(pane, false, Some(start)).await
        } else if let Some(n) = lines {
            let full = self.capture(pane, false, None).await?;
            let all: Vec<&str> = split_lines(&full);
            let from = all.len().saturating_sub(n as usize);
            Ok(all[from..].join("\n"))
        } else {
            self.capture(pane, false, None).await
        }
    }

    pub async fn display(&self, pane: &str, format: &str) -> anyhow::Result<String> {
        self.command(format!(
            "display-message -p -t {} {}",
            pane,
            shell_quote(format)
        ))
        .await
    }

    /// (cursor_x, cursor_y, alternate_on, pane_width, pane_height)
    pub async fn pane_meta(&self, pane: &str) -> anyhow::Result<(u16, u16, bool, u16, u16)> {
        let out = self
            .display(
                pane,
                "#{cursor_x} #{cursor_y} #{alternate_on} #{pane_width} #{pane_height}",
            )
            .await?;
        let p: Vec<&str> = out.split_whitespace().collect();
        if p.len() < 5 {
            return Err(anyhow!("unexpected pane meta: {out:?}"));
        }
        Ok((
            p[0].parse().unwrap_or(0),
            p[1].parse().unwrap_or(0),
            p[2] == "1",
            p[3].parse().unwrap_or(80),
            p[4].parse().unwrap_or(24),
        ))
    }

    /// Full screen snapshot as ANSI text that reproduces the pane in a fresh
    /// terminal, ending with the cursor positioned.
    pub async fn snapshot(&self, pane: &str) -> anyhow::Result<(u16, u16, String)> {
        let cap = self.capture(pane, true, None).await?;
        let (cx, cy, alt, cols, rows) = self.pane_meta(pane).await?;
        let mut out = String::with_capacity(cap.len() + 32);
        if alt {
            out.push_str("\x1b[?1049h\x1b[H");
        } else {
            out.push_str("\x1b[2J\x1b[H");
        }
        let lines = split_lines(&cap);
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            out.push_str(l);
        }
        // Erase the rest of the screen then position the cursor.
        out.push_str("\x1b[0J");
        out.push_str(&format!("\x1b[{};{}H", cy + 1, cx + 1));
        Ok((cols, rows, out))
    }

    pub async fn clear_history(&self, pane: &str) -> anyhow::Result<()> {
        self.command(format!("clear-history -t {pane}")).await?;
        Ok(())
    }

    pub async fn kill_window(&self, window: &str) -> anyhow::Result<()> {
        // Ignore "window not found" style errors; the goal is gone-ness.
        let _ = self.command(format!("kill-window -t {window}")).await;
        Ok(())
    }

    pub async fn window_exists(&self, window: &str) -> bool {
        self.display(window, "#{window_id}").await.is_ok()
    }

    pub async fn list_windows(&self) -> anyhow::Result<Vec<WinInfo>> {
        let fmt = "#{window_id}\t#{pane_id}\t#{@lmux_surface}\t#{@lmux_workspace}\t#{@lmux_title}\t#{window_width}\t#{window_height}\t#{pane_current_path}\t#{pane_current_command}\t#{@lmux_agent_session}";
        let out = self
            .command(format!(
                "list-windows -t {} -F {}",
                self.session_name(),
                shell_quote(fmt)
            ))
            .await?;
        let mut wins = Vec::new();
        for line in split_lines(&out) {
            if line.is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 7 {
                continue;
            }
            let opt = |s: &str| {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            };
            wins.push(WinInfo {
                window: f[0].to_string(),
                pane: f[1].to_string(),
                surface: opt(f[2]),
                workspace: opt(f[3]),
                title: opt(f[4]),
                cols: f[5].parse().unwrap_or(80),
                rows: f[6].parse().unwrap_or(24),
                cwd: f.get(7).and_then(|v| opt(v)),
                command: f.get(8).and_then(|v| opt(v)),
                agent_session: f.get(9).and_then(|v| opt(v)),
            });
        }
        Ok(wins)
    }

    pub async fn pane_exists(&self, pane: &str) -> bool {
        self.display(pane, "#{pane_id}").await.is_ok()
    }

    fn session_name(&self) -> String {
        self.session.clone()
    }
}

/// Split tmux capture output into screen lines, dropping only a single trailing
/// newline (interior blank lines are kept).
fn split_lines(s: &str) -> Vec<&str> {
    let trimmed = s.strip_suffix('\n').unwrap_or(s);
    if trimmed.is_empty() {
        return vec![];
    }
    trimmed.split('\n').collect()
}

async fn ensure_session(cfg: &TmuxConfig) -> anyhow::Result<()> {
    let has = Command::new("tmux")
        .args(["-L", &cfg.socket, "has-session", "-t", &cfg.session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("running tmux has-session")?;
    if has.success() {
        return Ok(());
    }
    let new = Command::new("tmux")
        .args([
            "-L",
            &cfg.socket,
            "new-session",
            "-d",
            "-s",
            &cfg.session,
            "-x",
            "200",
            "-y",
            "50",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running tmux new-session")?;
    if !new.status.success() {
        return Err(anyhow!(
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&new.stderr).trim()
        ));
    }
    // Sensible defaults for a private, headless server. Global option stays
    // `latest`; per-window `manual` is applied when each window is created.
    for args in [
        vec!["set-option", "-g", "status", "off"],
        vec!["set-option", "-g", "history-limit", "50000"],
        vec!["set-option", "-g", "destroy-unattached", "off"],
        vec!["set-option", "-g", "exit-empty", "off"],
        vec!["set-option", "-g", "window-size", "latest"],
    ] {
        let mut a = vec!["-L", &cfg.socket];
        a.extend(args);
        let _ = Command::new("tmux")
            .args(&a)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    Ok(())
}

/// tmux reads its own command syntax; wrap a value in double quotes and escape.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            '`' => out.push_str("\\`"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn sanitize_window_name(title: &str) -> String {
    let mut s: String = title
        .chars()
        .map(|c| match c {
            ':' | '.' | '\t' | '\n' | '\r' => '_',
            c => c,
        })
        .collect();
    if s.is_empty() {
        s = "term".to_string();
    }
    s.truncate(32);
    s
}

/// Keep one control client alive. If it exits (tmux server restart, broken pipe),
/// respawn it with backoff so streaming recovers on its own.
async fn supervisor(shared: Arc<Shared>, first_tx: oneshot::Sender<anyhow::Result<()>>) {
    let mut first = Some(first_tx);
    let mut backoff = Duration::from_millis(200);
    loop {
        // The server may have died (killed, or the machine rebooted under a
        // surviving daemon): recreate the session so `attach` has a target.
        if first.is_none() {
            let cfg = TmuxConfig {
                socket: shared.socket.clone(),
                session: shared.session.clone(),
            };
            if let Err(e) = ensure_session(&cfg).await {
                tracing::warn!("tmux: recreating session failed: {e}");
            }
        }
        let started = run_control_client(&shared, &mut first).await;
        // The client is gone (or never came up): mark not-ready and retry.
        shared.install(None).await;
        let _ = shared.events.send(TmuxEvent::ControlExited);
        if started {
            backoff = Duration::from_millis(200);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
}

/// Spawn one control client, wait for it to be ready, then wait for it to exit.
/// Returns `true` if it became ready at least once.
async fn run_control_client(
    shared: &Arc<Shared>,
    first: &mut Option<oneshot::Sender<anyhow::Result<()>>>,
) -> bool {
    let child = match Command::new("tmux")
        .args([
            "-L",
            &shared.socket,
            "-C",
            "attach",
            "-t",
            &shared.session,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            if let Some(tx) = first.take() {
                let _ = tx.send(Err(anyhow!("spawning tmux control client: {e}")));
            }
            return false;
        }
    };

    let (cmd_tx, cmd_rx) = mpsc::channel::<TmuxCmd>(256);
    let (ready_tx, ready_rx) = oneshot::channel::<anyhow::Result<()>>();
    let handle = tokio::spawn(reader_task(
        child,
        shared.events.clone(),
        cmd_rx,
        ready_tx,
    ));

    match tokio::time::timeout(Duration::from_secs(8), ready_rx).await {
        Ok(Ok(Ok(()))) => {
            shared.install(Some(cmd_tx)).await;
            if let Some(tx) = first.take() {
                let _ = tx.send(Ok(()));
            }
        }
        Ok(Ok(Err(e))) => {
            if let Some(tx) = first.take() {
                let _ = tx.send(Err(e));
            }
            handle.abort();
            return false;
        }
        Ok(Err(_)) => {
            if let Some(tx) = first.take() {
                let _ = tx.send(Err(anyhow!("tmux reader dropped during startup")));
            }
            return false;
        }
        Err(_) => {
            if let Some(tx) = first.take() {
                let _ = tx.send(Err(anyhow!("timed out waiting for tmux control client")));
            }
            handle.abort();
            return false;
        }
    }

    let _ = handle.await;
    true
}
/// Read the control stream as raw bytes and dispatch. Never fails on invalid
/// UTF-8: only a closed pipe or a read error ends the loop.
async fn reader_task(
    mut child: Child,
    events: mpsc::UnboundedSender<TmuxEvent>,
    mut cmd_rx: mpsc::Receiver<TmuxCmd>,
    ready_tx: oneshot::Sender<anyhow::Result<()>>,
) {
    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            let _ = ready_tx.send(Err(anyhow!("tmux control client has no stdin")));
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => BufReader::new(s),
        None => {
            let _ = ready_tx.send(Err(anyhow!("tmux control client has no stdout")));
            return;
        }
    };
    let mut reader = stdout;

    let mut pending: VecDeque<oneshot::Sender<anyhow::Result<String>>> = VecDeque::new();
    let mut block: Option<Vec<Vec<u8>>> = None;
    let mut ready = Some(ready_tx);
    let mut line: Vec<u8> = Vec::with_capacity(4096);

    loop {
        line.clear();
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        let mut l = cmd.line;
                        l.push('\n');
                        if stdin.write_all(l.as_bytes()).await.is_err() {
                            let _ = cmd.reply.send(Err(anyhow!("writing to tmux failed")));
                            break;
                        }
                        if stdin.flush().await.is_err() {
                            let _ = cmd.reply.send(Err(anyhow!("flushing tmux failed")));
                            break;
                        }
                        pending.push_back(cmd.reply);
                    }
                    None => break,
                }
            }
            n = reader.read_until(b'\n', &mut line) => {
                match n {
                    Ok(0) => break, // EOF: control client exited
                    Ok(_) => {
                        let mut end = line.len();
                        while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
                            end -= 1;
                        }
                        handle_line(&line[..end], &events, &mut block, &mut pending, &mut ready);
                    }
                    Err(_) => break,
                }
            }
        }
    }

    for tx in pending.drain(..) {
        let _ = tx.send(Err(anyhow!("tmux control client exited")));
    }
    if let Some(tx) = ready.take() {
        let _ = tx.send(Err(anyhow!("tmux control client exited during startup")));
    }
}

fn handle_line(
    line: &[u8],
    events: &mpsc::UnboundedSender<TmuxEvent>,
    block: &mut Option<Vec<Vec<u8>>>,
    pending: &mut VecDeque<oneshot::Sender<anyhow::Result<String>>>,
    ready: &mut Option<oneshot::Sender<anyhow::Result<()>>>,
) {
    if line.starts_with(b"%begin ") {
        // `%begin <time> <num> <flags>`; new reply block.
        *block = Some(Vec::new());
        return;
    }
    if line.starts_with(b"%end ") || line.starts_with(b"%error ") {
        let is_error = line.starts_with(b"%error ");
        let collected = block
            .take()
            .map(|lines| {
                lines
                    .iter()
                    .map(|l| String::from_utf8_lossy(l).into_owned())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if let Some(tx) = pending.pop_front() {
            if is_error {
                let _ = tx.send(Err(anyhow!("tmux: {}", collected.trim())));
            } else {
                let _ = tx.send(Ok(collected));
            }
        }
        if let Some(tx) = ready.take() {
            let _ = tx.send(Ok(()));
        }
        return;
    }
    if let Some(rest) = line.strip_prefix(b"%output ".as_slice()) {
        if let Some(pos) = rest.iter().position(|&b| b == b' ') {
            let pane = String::from_utf8_lossy(&rest[..pos]).into_owned();
            let data = unescape_output_bytes(&rest[pos + 1..]);
            let _ = events.send(TmuxEvent::Output { pane, data });
        }
        return;
    }
    if let Some(rest) = line.strip_prefix(b"%window-close ".as_slice()) {
        let _ = events.send(TmuxEvent::WindowClosed {
            window: String::from_utf8_lossy(rest).trim().to_string(),
        });
        return;
    }
    if let Some(rest) = line.strip_prefix(b"%unlinked-window-close ".as_slice()) {
        let _ = events.send(TmuxEvent::WindowClosed {
            window: String::from_utf8_lossy(rest).trim().to_string(),
        });
        return;
    }
    if line.starts_with(b"%exit") {
        // Control client detached; the read loop will end when stdout closes.
        return;
    }
    if let Some(b) = block.as_mut() {
        // Part of the current command reply (could be an empty line).
        b.push(line.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    async fn tmux_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Regression for review #1: a chunk of non-UTF-8 output must not kill the
    /// control client, and output must keep flowing afterwards.
    #[tokio::test]
    async fn control_client_survives_invalid_utf8_and_keeps_streaming() {
        if !tmux_available().await {
            eprintln!("skipping: tmux not available");
            return;
        }
        let socket = format!("lmux-test-{}", std::process::id());
        let cfg = TmuxConfig {
            socket: socket.clone(),
            session: "lmux".into(),
        };
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let tmux = match Tmux::spawn(cfg, ev_tx).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping: could not start tmux ({e})");
                return;
            }
        };

        // Emit one raw 0xff byte, then keep printing so we can see streaming resume.
        let (_win, _pane) = tmux
            .new_terminal(
                "/tmp",
                Some("printf '\\377'; while :; do echo AFTER_BINARY; sleep 0.2; done"),
                &[],
                80,
                24,
                "t",
                "surface:1",
                "workspace:1",
            )
            .await
            .expect("new_terminal");

        let mut saw_high_byte = false;
        let mut saw_after = false;
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(400), ev_rx.recv()).await {
                Ok(Some(TmuxEvent::Output { data, .. })) => {
                    if data.iter().any(|b| *b >= 0x80) {
                        saw_high_byte = true;
                    }
                    if data.windows(12).any(|w| w == b"AFTER_BINARY") {
                        saw_after = true;
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {}
            }
        }
        // The client must still accept commands after the binary chunk.
        let alive = tmux.list_windows().await.is_ok();

        let _ = Command::new("tmux")
            .args(["-L", &socket, "kill-server"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        assert!(saw_high_byte, "expected raw high bytes to pass through");
        assert!(saw_after, "expected output to continue after invalid UTF-8");
        assert!(alive, "control client should still accept commands");
    }

    /// A window titled like the session (a workspace named `lmux`) must not
    /// capture `-t lmux`: the next terminal is created at a free index, and its
    /// tags land on its own window, not on the namesake.
    #[tokio::test]
    async fn a_window_named_like_the_session_does_not_capture_new_terminals() {
        if !tmux_available().await {
            eprintln!("skipping: tmux not available");
            return;
        }
        let socket = format!("lmux-test-name-{}", std::process::id());
        let cfg = TmuxConfig { socket: socket.clone(), session: "lmux".into() };
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let tmux = match Tmux::spawn(cfg, ev_tx).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping: could not start tmux ({e})");
                return;
            }
        };
        let first = tmux
            .new_terminal("/tmp", None, &[], 80, 24, "lmux", "surface:1", "workspace:1")
            .await;
        let second = tmux
            .new_terminal("/tmp", None, &[], 80, 24, "other", "surface:2", "workspace:2")
            .await;
        let wins = tmux.list_windows().await;

        let _ = Command::new("tmux")
            .args(["-L", &socket, "kill-server"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        let (w1, _) = first.expect("first terminal");
        let (w2, _) = second.expect("second terminal, beside a window named like the session");
        let wins = wins.expect("list windows");
        let tag = |id: &str| wins.iter().find(|w| w.window == id).and_then(|w| w.surface.clone());
        assert_eq!(tag(&w1).as_deref(), Some("surface:1"));
        assert_eq!(tag(&w2).as_deref(), Some("surface:2"));
    }
}
