//! A real PTY on this machine, running a tmux client attached to one surface.
//!
//! Local terminal panes do not go through the daemon's websocket stream: the app
//! opens a PTY itself and runs
//!
//! ```text
//! tmux -L lmux new-session -A -t lmux -s view-<surface> ;
//!   set-option -t view-<surface> destroy-unattached on ;
//!   set-option -t view-<surface> status off ;
//!   select-window -t <tmux_pane> ;
//!   set-option -w -t <tmux_pane> window-size latest
//! ```
//!
//! in it. The tmux server (socket `lmux`, the one the daemon owns) still owns
//! the shell, its scrollback and its lifetime, so the pane keeps working and
//! survives the app quitting; only the attachment is local.
//!
//! The PTY is read on a background thread that hands byte chunks to the UI
//! thread over a channel and wakes egui, the same shape `net.rs` uses for
//! websocket messages. Writes (keys, paste) go straight to the PTY, which is
//! where the latency win comes from: no websocket, no daemon, no `send-keys`.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

/// The daemon's private tmux server (`crates/lmux-core/src/tmux.rs`,
/// `TmuxConfig::default`). `LMUX_TMUX_SOCKET` overrides it, matching the
/// daemon's own switch.
pub const DEFAULT_SOCKET: &str = "lmux";
/// The daemon's single session. View sessions are grouped with it.
const SESSION: &str = "lmux";

/// The tmux socket to attach to.
pub fn socket_name() -> String {
    std::env::var("LMUX_TMUX_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_owned())
}

/// tmux to run: the source build in `~/.local/bin` first, then Homebrew, then
/// the system paths, and finally a bare `tmux` for `PATH` to resolve.
pub fn tmux_bin() -> String {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/tmux"));
    }
    for p in ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux", "/usr/bin/tmux"] {
        candidates.push(std::path::PathBuf::from(p));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "tmux".to_owned())
}

/// Stop a scratch tmux server (tests and benches use their own `-L` socket)
/// and remove its socket file, which tmux leaves behind.
pub fn kill_scratch_server(tmux: &str, socket: &str) {
    let path = std::process::Command::new(tmux)
        .args(["-L", socket, "display-message", "-p", "#{socket_path}"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|p| !p.is_empty());
    let _ = std::process::Command::new(tmux)
        .args(["-L", socket, "kill-server"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

/// The tmux session a surface is viewed through. tmux rejects `:`, `.` and
/// whitespace in session names, so everything but alphanumerics becomes `-`.
pub fn view_name(surface: &str) -> String {
    let mut out = String::from("view-");
    for c in surface.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    out.truncate(40);
    out
}

/// The tmux argv that attaches a client to `tmux_pane`. Each `;` is its own
/// argv element, which is how tmux splits one command line.
///
/// `new-session -t lmux` makes the view session a *grouped* session, so two
/// visible panes each get their own current window and never fight over the
/// daemon session's. `-A` reuses an existing session instead of failing, which
/// makes the command idempotent. `destroy-unattached on` is set before anything
/// else so the view session disappears even if a later command fails.
/// `window-size latest` hands the window's size to this PTY: for a locally
/// attached pane the PTY drives the size, and the app sends no `TermResize`.
pub fn attach_argv(socket: &str, surface: &str, tmux_pane: &str) -> Vec<String> {
    let view = view_name(surface);
    vec![
        "-L".into(),
        socket.into(),
        "new-session".into(),
        "-A".into(),
        "-t".into(),
        SESSION.into(),
        "-s".into(),
        view.clone(),
        ";".into(),
        "set-option".into(),
        "-t".into(),
        view.clone(),
        "destroy-unattached".into(),
        "on".into(),
        ";".into(),
        "set-option".into(),
        "-t".into(),
        view,
        "status".into(),
        "off".into(),
        ";".into(),
        "select-window".into(),
        "-t".into(),
        tmux_pane.into(),
        ";".into(),
        "set-option".into(),
        "-w".into(),
        "-t".into(),
        tmux_pane.into(),
        "window-size".into(),
        "latest".into(),
    ]
}

/// A PTY owned by the app, plus the thread draining it.
pub struct LocalPty {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
    size: (u16, u16),
    /// The tmux pane this PTY was attached to (`None` for a plain command), so
    /// a surface whose window changed can be re-attached.
    pub tmux_pane: Option<String>,
}

impl LocalPty {
    /// Attach to `tmux_pane` through the daemon's tmux socket.
    pub fn spawn(
        surface: &str,
        tmux_pane: &str,
        cols: u16,
        rows: u16,
        wake: Option<egui::Context>,
    ) -> Result<Self, String> {
        Self::spawn_on(&socket_name(), surface, tmux_pane, cols, rows, wake)
    }

    /// Like [`Self::spawn`], with an explicit socket. Tests use this to attach
    /// to a scratch tmux server.
    pub fn spawn_on(
        socket: &str,
        surface: &str,
        tmux_pane: &str,
        cols: u16,
        rows: u16,
        wake: Option<egui::Context>,
    ) -> Result<Self, String> {
        let argv = attach_argv(socket, surface, tmux_pane);
        let mut pty = Self::spawn_argv(&tmux_bin(), &argv, cols, rows, wake)?;
        pty.tmux_pane = Some(tmux_pane.to_owned());
        Ok(pty)
    }

    /// Run an arbitrary program in a PTY. The benchmark uses this to measure the
    /// same read/write path without a tmux server in the way.
    pub fn spawn_argv(
        program: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        wake: Option<egui::Context>,
    ) -> Result<Self, String> {
        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows: rows.max(2),
            cols: cols.max(2),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| format!("open pty: {e}"))?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        // A GUI app is usually started without a usable TERM, and tmux refuses
        // to draw on a terminal it cannot name. `TMUX` must not leak in, or tmux
        // would think it is already inside a session.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env_remove("TMUX");
        if let Some(home) = dirs::home_dir() {
            cmd.cwd(home);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn {program}: {e}"))?;
        // The slave end is held by the child now; dropping ours means the master
        // reports EOF when the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().map_err(|e| format!("pty writer: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("pty reader: {e}"))?;

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("lmux-localpty".to_owned())
            .spawn(move || {
                let mut buf = [0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            if let Some(ctx) = &wake {
                                ctx.request_repaint();
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if let Some(ctx) = &wake {
                    ctx.request_repaint();
                }
            })
            .map_err(|e| format!("spawn pty reader: {e}"))?;

        Ok(Self {
            writer,
            master: pair.master,
            child,
            rx,
            size: (size.cols, size.rows),
            tmux_pane: None,
        })
    }

    /// Write key bytes or paste straight to the PTY.
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Resize the PTY. tmux gets SIGWINCH, and with `window-size latest` on the
    /// window it is this size that decides the pane's. Returns whether it
    /// changed.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let cols = cols.max(2);
        let rows = rows.max(2);
        if (cols, rows) == self.size {
            return false;
        }
        let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        if self.master.resize(size).is_err() {
            return false;
        }
        self.size = (cols, rows);
        true
    }

    /// Every chunk that arrived since the last call.
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => out.push(chunk),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    /// Block for the next chunk. Only the benchmark uses this: the app drains
    /// non-blockingly. `None` when the timeout expires.
    pub fn wait_chunk(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// False once the process underneath (the tmux client) has exited.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        // Closing the master already hangs up the slave, but kill the client
        // explicitly so the view session goes away now rather than when the
        // server next notices the socket close.
        let _ = self.child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_names_are_tmux_safe() {
        assert_eq!(view_name("surface:12"), "view-surface-12");
        assert_eq!(view_name("a.b:c"), "view-a-b-c");
        assert!(view_name("surface:1234567890123456789012345678901234567890").len() <= 40);
    }

    #[test]
    fn attach_argv_chains_into_one_tmux_invocation() {
        let argv = attach_argv("lmux", "surface:3", "%12");
        let line = argv.join(" ");
        assert!(line.starts_with("tmux ") || line.starts_with("-L "));
        assert!(line.contains("new-session -A -t lmux -s view-surface-3"));
        assert!(line.contains("set-option -t view-surface-3 destroy-unattached on"));
        assert!(line.contains("set-option -t view-surface-3 status off"));
        assert!(line.contains("select-window -t %12"));
        assert!(line.contains("set-option -w -t %12 window-size latest"));
        // The chained commands are separate argv elements, which is how tmux
        // splits one command line.
        assert_eq!(argv.iter().filter(|a| a.as_str() == ";").count(), 4);
    }

    #[test]
    fn a_pty_round_trips_bytes() {
        let Ok(mut pty) = LocalPty::spawn_argv(
            "/bin/sh",
            &["-c".to_owned(), "stty raw; exec cat".to_owned()],
            80,
            24,
            None,
        ) else {
            eprintln!("skipping: could not open a PTY");
            return;
        };
        pty.write(b"hello").expect("write");
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while seen.len() < 5 && std::time::Instant::now() < deadline {
            if let Some(chunk) = pty.wait_chunk(Duration::from_millis(200)) {
                seen.extend_from_slice(&chunk);
            }
        }
        assert!(seen.starts_with(b"hello"), "echoed {seen:?}");
        assert!(pty.is_alive());
    }
}
