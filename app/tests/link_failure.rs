//! Failure injection against a real, isolated daemon: the app's `Net` and
//! `AppState` (the same pump/flush the app runs each frame) connect to a
//! scratch `lmux server` as a "direct" host while the test stops, resumes,
//! kills and restarts that daemon, and reattaches while a pane streams output.
//!
//! The daemon gets its own HOME, TMPDIR, unix socket, state dir, tmux socket
//! and websocket port; nothing here touches a real lmux. Skipped (with a note)
//! when tmux or the `lmux` binary (`cargo build -p lmux` at the repo root, or
//! `LMUX_TEST_BIN`) is missing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lmux_app::net::{ConnState, ErrorClass, HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::wire::{self, ClientMsg, Frame, ServerMsg};

fn lmux_bin() -> Option<PathBuf> {
    let p = std::env::var_os("LMUX_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/lmux"));
    p.is_file().then_some(p)
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Daemon {
    bin: PathBuf,
    child: Option<Child>,
    dir: tempfile::TempDir,
    tmux_socket: String,
    port: u16,
}

impl Daemon {
    fn base_cmd(bin: &Path, dir: &Path) -> Command {
        let mut c = Command::new(bin);
        c.env("HOME", dir.join("home"))
            .env("TMPDIR", dir.join("tmp"))
            .env("TMUX_TMPDIR", dir.join("tmp"))
            .env_remove("LMUX_SOCKET")
            .env_remove("LMUX_WORKSPACE_ID")
            .env_remove("LMUX_SURFACE_ID")
            .env_remove("LMUX_WS_URL")
            .env_remove("TMUX");
        c
    }

    fn start() -> Option<Daemon> {
        let Some(bin) = lmux_bin() else {
            eprintln!("skipping: no lmux binary (cargo build -p lmux)");
            return None;
        };
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return None;
        }
        // Short path: unix socket paths are limited to ~100 bytes on macOS.
        let dir = tempfile::Builder::new().prefix("lmuxlf").tempdir_in("/tmp").unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        // One tmux server per daemon: the tests in this file run in parallel.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmux_socket = format!("lmux-lf-it-{}-{n}", std::process::id());
        let mut d = Daemon { bin, child: None, dir, tmux_socket, port: free_port() };
        d.spawn();
        Some(d)
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("s")
    }

    fn spawn(&mut self) {
        let child = Self::base_cmd(&self.bin, self.dir.path())
            .arg("--socket")
            .arg(self.socket())
            .arg("server")
            .arg("--state-dir")
            .arg(self.dir.path().join("state"))
            .args(["--tmux-socket", &self.tmux_socket, "--no-browser", "--ws-addr", &format!("127.0.0.1:{}", self.port)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lmux server");
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.cli(&["ping"]).is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("daemon did not answer ping");
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("daemon running").id()
    }

    fn signal(&self, sig: &str) {
        // Only the pid this test started.
        let ok = Command::new("kill").args([sig, &self.pid().to_string()]).status().unwrap().success();
        assert!(ok, "kill {sig}");
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn cli(&self, args: &[&str]) -> Option<String> {
        let out = Self::base_cmd(&self.bin, self.dir.path())
            .arg("--socket")
            .arg(self.socket())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn host(&self) -> HostConfig {
        HostConfig {
            name: "scratch".into(),
            label: "scratch".into(),
            kind: "direct".into(),
            url: Some(format!("ws://127.0.0.1:{}", self.port)),
            ssh_target: None,
            remote_port: 7878,
            browser_scale: None,
            stream_browsers: false,
            cloud: None,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_ref() {
            // A stopped process would not see the kill until continued.
            let _ = Command::new("kill").args(["-CONT", &c.id().to_string()]).status();
        }
        self.kill();
        let _ = Command::new("tmux")
            .args(["-L", &self.tmux_socket, "kill-server"])
            .env("TMUX_TMPDIR", self.dir.path().join("tmp"))
            .env("TMPDIR", self.dir.path().join("tmp"))
            .env_remove("TMUX")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// What the app does each frame, minus the window: poll, apply, subscribe, send.
struct Client {
    net: Net,
    state: AppState,
    ctx: egui::Context,
    surface: String,
    /// Wanted on screen; false stops the subscription.
    show: bool,
    was_connected: bool,
    snapshots: u32,
    subscribes: u32,
    unsubscribes: u32,
    /// Numbers `T<n>` seen in the stream since the last (re)connect.
    seen: Vec<u64>,
    connects: u32,
}

impl Client {
    fn new(host: HostConfig, surface: &str) -> Client {
        let mut net = Net::for_host(vec![host], 0);
        let ctx = egui::Context::default();
        net.set_wake(ctx.clone());
        let mut state = AppState::new();
        state.set_wake(ctx.clone());
        state.desired_sizes.insert(surface.to_owned(), (80, 24));
        Client {
            net,
            state,
            ctx,
            surface: surface.to_owned(),
            show: true,
            was_connected: false,
            snapshots: 0,
            subscribes: 0,
            unsubscribes: 0,
            seen: Vec::new(),
            connects: 0,
        }
    }

    fn step(&mut self) {
        self.state.set_current_host(&self.net.host_name());
        for msg in self.net.poll(&self.ctx) {
            match msg {
                ewebsock::WsMessage::Text(t) => {
                    if let Some(sm) = wire::decode_server(&t) {
                        if matches!(sm, ServerMsg::TermSnapshot { .. }) {
                            self.snapshots += 1;
                        }
                        self.state.handle_server_msg(sm);
                    }
                }
                ewebsock::WsMessage::Binary(b) => {
                    if let Ok(Frame::TermOutput { data, .. }) = Frame::decode(&b) {
                        collect_numbers(&String::from_utf8_lossy(data), &mut self.seen);
                    }
                    self.state.handle_frame(&b);
                }
                _ => {}
            }
        }
        let connected = self.net.state == ConnState::Connected;
        if !connected && self.was_connected {
            self.state.reset_subscriptions();
        }
        if connected && !self.was_connected {
            self.connects += 1;
            self.seen.clear();
        }
        self.was_connected = connected;
        if self.show && self.state.surface(&self.surface).is_some() {
            self.state.ensure_term(&self.surface);
            self.state.visible.insert(self.surface.clone());
        }
        self.state.sync_subscriptions(connected);
        for msg in std::mem::take(&mut self.state.pending) {
            match &msg {
                ClientMsg::Subscribe { surface, .. } if *surface == self.surface => self.subscribes += 1,
                ClientMsg::Unsubscribe { surface } if *surface == self.surface => self.unsubscribes += 1,
                _ => {}
            }
            self.net.send(&msg);
        }
    }

    /// Run frames until `done` or `timeout`; returns how long it took.
    fn until(&mut self, what: &str, timeout: Duration, mut done: impl FnMut(&mut Client) -> bool) -> Duration {
        let start = Instant::now();
        loop {
            self.step();
            if done(self) {
                return start.elapsed();
            }
            assert!(
                start.elapsed() < timeout,
                "timed out after {timeout:?} waiting for {what}: state={:?} error={:?} attempt={}",
                self.net.state,
                self.net.last_error,
                self.net.attempt()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn run_for(&mut self, d: Duration) {
        let start = Instant::now();
        while start.elapsed() < d {
            self.step();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn streaming(&self) -> bool {
        self.state.terms.get(&self.surface).is_some_and(|t| t.subscribed) && self.snapshots > 0
    }
}

/// Every `T<digits>` token in `text`, in order.
fn collect_numbers(text: &str, out: &mut Vec<u64>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'T' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            // A token cut off by the end of the chunk is not a whole number.
            if j < b.len() || text.ends_with(['\r', '\n']) {
                if let Ok(n) = text[i + 1..j].parse() {
                    out.push(n);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
}

fn assert_no_duplicates(seen: &[u64], what: &str) {
    let uniq: BTreeSet<_> = seen.iter().collect();
    assert_eq!(uniq.len(), seen.len(), "{what}: output delivered twice: {seen:?}");
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "{what}: output out of order: {seen:?}");
}

/// A daemon with one workspace whose shell prints `T0`, `T1`, … ten times a second.
fn streaming_daemon() -> Option<(Daemon, Client)> {
    let d = Daemon::start()?;
    let work = d.dir.path().join("home");
    let script = "i=0; while :; do echo T$i; i=$((i+1)); sleep 0.1; done";
    d.cli(&["new-workspace", "--name", "a", "--cwd", work.to_str().unwrap(), "--command", "/bin/sh"])
        .expect("new-workspace");
    d.cli(&["send", "--surface", "surface:1", &format!("{script}\\n")]).expect("send");
    let mut c = Client::new(d.host(), "surface:1");
    c.net.begin_connect(&c.ctx.clone());
    c.until("first snapshot", Duration::from_secs(20), |c| c.streaming());
    c.until("streamed output", Duration::from_secs(10), |c| c.seen.len() >= 3);
    Some((d, c))
}

#[test]
fn a_stopped_daemon_is_declared_dead_quickly_and_recovers_on_continue() {
    let Some((d, mut c)) = streaming_daemon() else { return };
    assert_eq!(c.net.state, ConnState::Connected);

    d.signal("-STOP");
    // 2 s idle ping + 3 s without a pong.
    let took = c.until("the link to be declared dead", Duration::from_secs(10), |c| c.net.state != ConnState::Connected);
    assert!(took <= Duration::from_secs(8), "declared dead after {took:?}");
    assert!(c.net.last_error.as_deref().unwrap_or("").contains("timed out"), "{:?}", c.net.last_error);
    assert_eq!(c.net.attention, None, "a stalled link is transient");

    d.signal("-CONT");
    c.until("recovery after SIGCONT", Duration::from_secs(40), |c| c.net.state == ConnState::Connected && c.streaming() && c.snapshots >= 2);
    c.until("output after recovery", Duration::from_secs(10), |c| c.seen.len() >= 3);
    assert_no_duplicates(&c.seen, "after SIGCONT");
    assert_eq!(c.subscribes, c.connects, "one subscribe per connection");
}

#[test]
fn a_killed_daemon_is_reconnected_and_resubscribed() {
    let Some((mut d, mut c)) = streaming_daemon() else { return };
    let before = c.connects;

    d.kill();
    c.until("the link to drop", Duration::from_secs(10), |c| c.net.state != ConnState::Connected);
    // Down for a while: the delays grow and nothing connects.
    c.run_for(Duration::from_secs(2));
    assert_ne!(c.net.state, ConnState::Connected);
    assert!(c.net.attempt() >= 2, "attempt={}", c.net.attempt());

    // Same state dir and tmux socket: the panes outlived the daemon.
    d.spawn();
    c.net.retry_soon();
    c.until("reconnect", Duration::from_secs(40), |c| c.net.state == ConnState::Connected && c.connects > before);
    let snaps = c.snapshots;
    c.until("resubscribed snapshot", Duration::from_secs(20), |c| c.snapshots > snaps || c.streaming() && c.seen.len() >= 3);
    c.until("output after the restart", Duration::from_secs(15), |c| c.seen.len() >= 3);
    assert_no_duplicates(&c.seen, "after the restart");
    assert!(c.state.terms.get("surface:1").is_some_and(|t| t.subscribed));
    assert_eq!(c.subscribes, c.connects, "one subscribe per connection");
}

#[test]
fn repeated_reattach_while_streaming_keeps_exactly_one_subscription() {
    let Some((_d, mut c)) = streaming_daemon() else { return };
    for round in 0..6 {
        let connects = c.connects;
        c.net.reconnect_now(&c.ctx.clone());
        c.until("reattach", Duration::from_secs(20), |c| c.net.state == ConnState::Connected && c.connects > connects && c.seen.len() >= 3);
        assert_no_duplicates(&c.seen, &format!("round {round}"));
        assert_eq!(c.net.attempt(), 0, "a manual refresh starts the backoff over");
    }
    assert_eq!(c.subscribes, c.connects, "one subscribe per connection, none lost");
    assert_eq!(c.snapshots, c.connects, "one snapshot per subscribe");

    // Hiding and showing the pane swaps one subscription for one.
    c.show = false;
    c.state.visible.clear();
    c.until("unsubscribe", Duration::from_secs(5), |c| c.unsubscribes >= 1);
    let (subs, unsubs) = (c.subscribes, c.unsubscribes);
    c.run_for(Duration::from_millis(500));
    assert_eq!((c.subscribes, c.unsubscribes), (subs, unsubs), "no repeats while hidden");
}

#[test]
fn a_hello_with_another_protocol_version_stops_the_retries() {
    let Some((_d, mut c)) = streaming_daemon() else { return };
    c.net.send(&ClientMsg::Hello { protocol_version: 999, client_name: "lmux-app/future".into(), draws_browsers: false });
    c.until("the needs-update state", Duration::from_secs(10), |c| c.net.needs_update());
    assert_eq!(c.net.state, ConnState::Disconnected);
    assert_eq!(c.net.attention, Some(ErrorClass::Version));
    assert!(c.net.last_error.as_deref().unwrap().starts_with("needs update"), "{:?}", c.net.last_error);
    // No retry loop: it stays down.
    let connects = c.connects;
    c.run_for(Duration::from_secs(3));
    assert_eq!(c.net.state, ConnState::Disconnected);
    assert_eq!(c.connects, connects);
    c.net.retry_soon();
    c.run_for(Duration::from_millis(500));
    assert_eq!(c.net.state, ConnState::Disconnected);

    // Refresh Connection (or a finished update) tries again.
    c.net.reconnect_now(&c.ctx.clone());
    c.until("reconnect after refresh", Duration::from_secs(20), |c| c.net.state == ConnState::Connected);
    assert_eq!(c.net.attention, None);
}
