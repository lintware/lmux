//! Transport: host config, SSH tunnel and the websocket client with backoff.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use ewebsock::{Options, WsEvent, WsMessage, WsReceiver, WsSender};
use serde::{Deserialize, Serialize};

use crate::wire::{self, encode, m, ClientMsg, ServerMsg, PROTOCOL_VERSION};

pub const DEFAULT_REMOTE_PORT: u16 = 7878;
pub const MOCK_URL: &str = "ws://127.0.0.1:7900";
/// The daemon the app runs on this machine, and its websocket address.
pub const LOCAL_WS_ADDR: &str = "127.0.0.1:7879";
pub const LOCAL_HOST_NAME: &str = "this mac";

/// How often to ping while connected, and how long to wait for the pong before
/// declaring the link dead (review #14). A dead link is then noticed in a few
/// seconds rather than waiting out `ServerAliveInterval 15 × 3` ≈ 45 s.
const PING_INTERVAL: Duration = Duration::from_secs(2);
const PING_TIMEOUT: Duration = Duration::from_secs(3);
/// Ping RPC ids live well above the UI's own ids so they never collide.
const PING_ID_BASE: u64 = 1_000_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub name: String,
    pub label: String,
    /// "ssh" tunnels the remote unix socket; "direct" is a plain websocket URL.
    pub kind: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub ssh_target: Option<String>,
    #[serde(default = "default_remote_port")]
    pub remote_port: u16,
    /// Browser stream device scale factor. `None` (the default) streams at 1×,
    /// which is 4× fewer pixels than a Retina 2× stream (review #9). Set 2.0 on
    /// a fast link; `LMUX_BROWSER_SCALE` overrides for a run.
    #[serde(default)]
    pub browser_scale: Option<f32>,
    /// Stream this host's browser tabs from its Chromium, instead of running
    /// them in the app (see [`Net::draws_browsers`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream_browsers: bool,
    /// Set for a cloud box lmux created, so the app can delete it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudBox>,
}

/// A cloud box lmux created (`deploy/cloud/<provider>.sh`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudBox {
    /// `aws` or `gcp`.
    pub provider: String,
    /// The box's name; its SSH alias is `lmux-<name>`.
    pub name: String,
    /// AWS region or GCP zone.
    pub region: String,
}

fn default_remote_port() -> u16 {
    DEFAULT_REMOTE_PORT
}

/// Hosts a first run starts with. "this mac" is added by
/// [`load_hosts_with_local`]; remote hosts come from Add Remote Host….
pub fn default_hosts() -> Vec<HostConfig> {
    Vec::new()
}

pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("lmux").join("hosts.json"))
}

/// A host entry for the daemon on this machine. Derived, never written to
/// `hosts.json`: its port is the app's own choice.
fn local_host() -> HostConfig {
    HostConfig {
        name: LOCAL_HOST_NAME.into(),
        label: LOCAL_HOST_NAME.into(),
        kind: "direct".into(),
        url: Some(format!("ws://{LOCAL_WS_ADDR}")),
        ssh_target: None,
        remote_port: DEFAULT_REMOTE_PORT,
        browser_scale: None,
        stream_browsers: false,
        cloud: None,
    }
}

/// The app's own `Contents/MacOS` when it ships tmux there (a packaged
/// lmux.app does, so a fresh Mac needs no Homebrew); `None` in a dev build.
pub fn bundled_bin_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join("tmux").is_file().then_some(dir)
}

/// PATH for the daemon we spawn: the bundled tmux first, else a source build
/// in `~/.local/bin` or a Homebrew one, none of which is guaranteed to be in
/// the app's own environment when it is launched from Finder.
pub fn daemon_path() -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(dir) = bundled_bin_dir() {
        parts.push(dir.display().to_string());
    }
    if let Some(home) = dirs::home_dir() {
        parts.push(home.join(".local/bin").display().to_string());
        parts.push(home.join(".cargo/bin").display().to_string());
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        parts.push(dir.to_owned());
    }
    if let Ok(path) = std::env::var("PATH") {
        parts.push(path);
    }
    parts.join(":")
}

/// The `lmux` binary to run as the local daemon: `$LMUX_SERVER_BIN`, then one
/// beside the app (a repo build leaves `lmux` and `lmux-app` side by side),
/// then the first `lmux` on the daemon PATH.
pub fn local_server_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LMUX_SERVER_BIN") {
        return Some(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("lmux");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    daemon_path()
        .split(':')
        .map(|d| PathBuf::from(d).join("lmux"))
        .find(|c| c.is_file())
}

fn tmux_available() -> bool {
    daemon_path()
        .split(':')
        .any(|d| PathBuf::from(d).join("tmux").is_file())
}

/// Is something already serving the local daemon's port?
fn local_daemon_up() -> bool {
    let Ok(addr) = LOCAL_WS_ADDR.parse() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Start the daemon for this machine unless one is already listening. An app
/// restart therefore reconnects to the daemon it left behind, so local sessions
/// outlive the window. The child is deliberately not owned by the app: it keeps
/// running (with its tmux server) after the app quits.
pub fn ensure_local_daemon() -> Result<(), String> {
    if local_daemon_up() {
        return Ok(());
    }
    let bin = local_server_bin().ok_or_else(|| {
        "local daemon: no `lmux` binary found (build it, or set LMUX_SERVER_BIN)".to_owned()
    })?;
    if !tmux_available() {
        return Err("local daemon needs tmux on PATH (install it, or set LMUX_SERVER_BIN)".to_owned());
    }
    let mut cmd = Command::new(&bin);
    cmd.arg("server").arg("--ws-addr").arg(LOCAL_WS_ADDR);
    // Browser panes here are native WebKit views in the app (`webview`), so
    // the daemon runs no Chromium.
    cmd.arg("--app-browser");
    // The daemon outlives the app; its log goes beside its state.
    let log = dirs::home_dir()
        .map(|h| h.join(".local/state/lmux"))
        .and_then(|d| std::fs::create_dir_all(&d).ok().map(|_| d.join("daemon.log")))
        .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
    let (out, err) = match log.and_then(|f| f.try_clone().ok().map(|g| (f, g))) {
        Some((f, g)) => (Stdio::from(f), Stdio::from(g)),
        None => (Stdio::null(), Stdio::null()),
    };
    cmd.env("PATH", daemon_path())
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    for _ in 0..50 {
        if local_daemon_up() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("local daemon did not answer on {LOCAL_WS_ADDR}"))
}

/// Hosts to offer, with the local daemon first. `LMUX_WS_URL` means a single
/// explicit host for development, and no local daemon at all.
pub fn load_hosts_with_local() -> Vec<HostConfig> {
    let mut hosts = load_hosts();
    if std::env::var("LMUX_WS_URL").is_ok() {
        return hosts;
    }
    if !hosts.iter().any(|h| h.name == LOCAL_HOST_NAME) {
        hosts.insert(0, local_host());
    }
    hosts
}

/// Where an "install lmux on this host" job stands.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallStatus {
    /// Running; the installer's latest progress line.
    Running(String),
    Done,
    /// Failed; the last lines it printed.
    Failed(String),
}

/// The installer script and, inside the app bundle, the source it ships:
/// `Contents/Resources/{install-remote.sh,lmux-src.tar}`. From a checkout
/// (development), the repo's own script, which ships the source from git.
pub fn installer() -> Option<(PathBuf, Option<PathBuf>)> {
    if let Some(res) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().and_then(|d| d.parent()).map(|c| c.join("Resources")))
    {
        let (script, tar) = (res.join("install-remote.sh"), res.join("lmux-src.tar"));
        if script.is_file() && tar.is_file() {
            return Some((script, Some(tar)));
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/install-remote.sh");
    dev.is_file().then_some((dev, None))
}

/// Install (or update) lmux on `target` over SSH in the background: build it
/// there, run it as the lmuxd user service, add the Claude hooks. The log
/// goes to ~/.local/state/lmux/install-<target>.log.
pub fn start_install(target: &str, wake: Option<egui::Context>) -> std::sync::Arc<std::sync::Mutex<InstallStatus>> {
    let t = target.to_owned();
    start_job(&format!("install-{target}"), wake, move || {
        let (script, tar) = installer().ok_or("this build has no installer")?;
        let mut cmd = Command::new("bash");
        cmd.arg(&script).arg(&t);
        if let Some(tar) = &tar {
            cmd.env("LMUX_SRC_TAR", tar);
        }
        Ok(cmd)
    })
}

/// The cloud scripts (`deploy/cloud/<provider>.sh`): in the bundle's
/// Resources/cloud, or the repo's when running from a checkout.
pub fn cloud_script(provider: &str) -> Option<PathBuf> {
    let name = format!("{provider}.sh");
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().and_then(|d| d.parent()).map(|c| c.join("Resources/cloud").join(&name)));
    if let Some(p) = bundled.filter(|p| p.is_file()) {
        return Some(p);
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/cloud").join(&name);
    dev.is_file().then_some(dev)
}

/// Create a cloud box (`<provider>.sh create`), then install lmux on it, in
/// the background. The box is reachable as the SSH alias `lmux-<name>`.
pub fn start_cloud_create(
    provider: &str,
    name: &str,
    region: &str,
    size: &str,
    wake: Option<egui::Context>,
) -> std::sync::Arc<std::sync::Mutex<InstallStatus>> {
    let (provider, name, region, size) = (provider.to_owned(), name.to_owned(), region.to_owned(), size.to_owned());
    start_job(&format!("cloud-{name}"), wake, move || {
        let script = cloud_script(&provider).ok_or("this build has no cloud scripts")?;
        let (installer_script, tar) = installer().ok_or("this build has no installer")?;
        // Positional args keep every value out of the shell's parsing.
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(r#"bash "$0" create "$1" "$2" "$3" && bash "$4" "lmux-$1""#)
            .arg(&script)
            .arg(&name)
            .arg(&region)
            .arg(&size)
            .arg(&installer_script);
        if let Some(tar) = &tar {
            cmd.env("LMUX_SRC_TAR", tar);
        }
        Ok(cmd)
    })
}

/// Delete a cloud box made by `start_cloud_create`.
pub fn start_cloud_delete(
    provider: &str,
    name: &str,
    region: &str,
    wake: Option<egui::Context>,
) -> std::sync::Arc<std::sync::Mutex<InstallStatus>> {
    let (provider, name, region) = (provider.to_owned(), name.to_owned(), region.to_owned());
    start_job(&format!("cloud-delete-{name}"), wake, move || {
        let script = cloud_script(&provider).ok_or("this build has no cloud scripts")?;
        let mut cmd = Command::new("bash");
        cmd.arg(&script).arg("delete").arg(&name).arg(&region);
        Ok(cmd)
    })
}

/// Run a command in the background, streaming its output lines as progress
/// and its log to ~/.local/state/lmux/<log_name>.log.
fn start_job(
    log_name: &str,
    wake: Option<egui::Context>,
    build: impl FnOnce() -> Result<Command, &'static str> + Send + 'static,
) -> std::sync::Arc<std::sync::Mutex<InstallStatus>> {
    use std::io::BufRead;
    let status = std::sync::Arc::new(std::sync::Mutex::new(InstallStatus::Running("starting…".into())));
    let out = status.clone();
    let target: String = log_name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
    std::thread::spawn(move || {
        let set = |s: InstallStatus| {
            if let Ok(mut g) = out.lock() {
                *g = s;
            }
            if let Some(c) = &wake {
                c.request_repaint();
            }
        };
        let mut cmd = match build() {
            Ok(c) => c,
            Err(e) => {
                set(InstallStatus::Failed(e.into()));
                return;
            }
        };
        let log_path = dirs::home_dir().map(|h| h.join(format!(".local/state/lmux/{target}.log")));
        let mut log = log_path.as_ref().and_then(|p| std::fs::File::create(p).ok());
        // The cloud CLIs (aws, gcloud) live in Homebrew and the user's bins.
        cmd.env("PATH", format!("{}:/opt/homebrew/bin:/usr/local/bin", daemon_path()));
        let child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                set(InstallStatus::Failed(format!("could not run the installer: {e}")));
                return;
            }
        };
        // stderr is folded into the same stream of progress lines.
        let err = child.stderr.take();
        let err_lines = std::thread::spawn(move || {
            err.map(|e| std::io::BufReader::new(e).lines().map_while(Result::ok).collect::<Vec<_>>()).unwrap_or_default()
        });
        let mut tail: Vec<String> = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(f) = log.as_mut() {
                    let _ = writeln!(f, "{line}");
                }
                set(InstallStatus::Running(line.trim_start_matches("==> ").to_owned()));
                tail.push(line);
            }
        }
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        for line in err_lines.join().unwrap_or_default() {
            if let Some(f) = log.as_mut() {
                let _ = writeln!(f, "{line}");
            }
            tail.push(line);
        }
        if ok {
            set(InstallStatus::Done);
        } else {
            let last: Vec<String> = tail.iter().rev().take(3).rev().cloned().collect();
            set(InstallStatus::Failed(last.join(" · ")));
        }
    });
    status
}

/// The hosts the app connects to at once: this machine first, then every
/// configured remote. The bun mock is a development stand-in, so it is left
/// out unless `LMUX_WS_URL` pins the app to a single host.
pub fn app_hosts() -> Vec<HostConfig> {
    let hosts = load_hosts_with_local();
    if std::env::var("LMUX_WS_URL").is_ok() {
        return hosts;
    }
    hosts.into_iter().filter(|h| !(h.kind == "direct" && h.name == "mock")).collect()
}

/// A remote host reached over SSH by `target` (an alias from ~/.ssh/config or
/// `user@host`). Its daemon listens on the default port behind the tunnel.
pub fn ssh_host(target: &str) -> HostConfig {
    HostConfig {
        name: target.to_owned(),
        label: target.to_owned(),
        kind: "ssh".into(),
        url: None,
        ssh_target: Some(target.to_owned()),
        remote_port: DEFAULT_REMOTE_PORT,
        browser_scale: None,
        stream_browsers: false,
        cloud: None,
    }
}

/// Write the configured hosts back to `hosts.json`. The local host is derived
/// at runtime and never written.
pub fn save_hosts(hosts: &[HostConfig]) -> Result<(), String> {
    let path = config_path().ok_or("no config directory")?;
    let keep: Vec<&HostConfig> = hosts.iter().filter(|h| h.name != LOCAL_HOST_NAME).collect();
    let json = serde_json::to_string_pretty(&keep).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Load hosts, seeding the config file on first run. `LMUX_WS_URL` overrides
/// everything and points at a single direct host (used for development).
pub fn load_hosts() -> Vec<HostConfig> {
    if let Ok(url) = std::env::var("LMUX_WS_URL") {
        return vec![HostConfig {
            name: "env".into(),
            label: "LMUX_WS_URL".into(),
            kind: "direct".into(),
            url: Some(url),
            ssh_target: None,
            remote_port: DEFAULT_REMOTE_PORT,
            browser_scale: None,
            stream_browsers: false,
            cloud: None,
        }];
    }
    let Some(path) = config_path() else {
        return default_hosts();
    };
    if !path.exists() {
        let seeded = default_hosts();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&seeded) {
            if let Ok(mut f) = std::fs::File::create(&path) {
                let _ = f.write_all(json.as_bytes());
            }
        }
        return seeded;
    }
    match std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        Some(hosts) => hosts,
        None => default_hosts(),
    }
}

// --- tunnel ----------------------------------------------------------------

pub struct Tunnel {
    child: Option<Child>,
    pub port: u16,
    /// The host's network as a SOCKS5 proxy (`ssh -D`), for browser pages
    /// this app runs on the host's behalf.
    pub socks_port: Option<u16>,
    /// What the shared master must forget when this tunnel goes.
    target: String,
    sock: PathBuf,
    forward: String,
}

impl Tunnel {
    /// Stop the forwarding ssh and cancel its forwards in the shared master,
    /// returning once the master has forgotten them. Quit uses this so the
    /// cancel reaches the master before the master is told to exit.
    fn close(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            run_cancel(cancel_args(&self.target, &self.sock, &self.forward, self.socks_port));
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // The cancel is a round trip to the master; keep it off the UI thread.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let args = cancel_args(&self.target, &self.sock, &self.forward, self.socks_port);
            std::thread::spawn(move || {
                let _ = child.wait();
                run_cancel(args);
            });
        }
    }
}

fn run_cancel(args: Vec<String>) {
    let _ = Command::new("ssh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `ssh` arguments that cancel a tunnel's `-L` and `-D` forwards in the master
/// behind `sock`.
fn cancel_args(target: &str, sock: &std::path::Path, forward: &str, socks_port: Option<u16>) -> Vec<String> {
    let mut a = vec!["-O".to_owned(), "cancel".into(), "-L".into(), forward.to_owned()];
    if let Some(s) = socks_port {
        a.push("-D".into());
        a.push(format!("127.0.0.1:{s}"));
    }
    a.extend(["-o".into(), format!("ControlPath={}", sock.display()), "-o".into(), "BatchMode=yes".into(), target.to_owned()]);
    a
}

fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

fn port_open(port: u16) -> bool {
    let Ok(addr) = format!("127.0.0.1:{port}").parse() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

fn wait_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if port_open(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// A stable control socket per ssh target, so forwards (and reconnects) to the
/// same host multiplex over one transport (review #14).
pub fn control_path(target: &str) -> PathBuf {
    let mut h = DefaultHasher::new();
    target.hash(&mut h);
    std::env::temp_dir().join(format!("lmux-ssh-{:016x}", h.finish()))
}

/// Whether the master behind `sock` still answers (`ssh -O check`).
fn master_alive(target: &str, sock: &std::path::Path) -> bool {
    Command::new("ssh")
        .args(["-O", "check"])
        .arg("-o")
        .arg(format!("ControlPath={}", sock.display()))
        .arg("-o")
        .arg("BatchMode=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg(target)
        .status()
        .is_ok_and(|s| s.success())
}

/// Make sure a background SSH master exists for `target`. `ControlPersist`
/// keeps the transport alive between tunnels, so the next `open_tunnel` skips
/// the handshake and returns almost instantly.
fn ensure_master(target: &str) -> Result<PathBuf, String> {
    let sock = control_path(target);
    if sock.exists() {
        // A socket file outlives a dead connection (network change, sleep),
        // and every tunnel through it would fail until ControlPersist ran out.
        if master_alive(target, &sock) {
            return Ok(sock);
        }
        let _ = std::fs::remove_file(&sock);
    }
    // ssh forks into the background and keeps its stderr, so a pipe would
    // never reach EOF; a file holds what it said before it forked.
    let err_path = sock.with_extension("err");
    let err_file = std::fs::File::create(&err_path).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
    let status = Command::new("ssh")
        .args(["-fN", "-M"])
        .arg("-o")
        .arg("ConnectTimeout=8")
        .arg("-o")
        .arg(format!("ControlPath={}", sock.display()))
        .arg("-o")
        .arg("ControlPersist=60s")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err_file)
        .arg(target)
        .status()
        .map_err(|e| format!("spawn ssh master: {e}"))?;
    let said = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    if !status.success() {
        let said = said.trim();
        return Err(if said.is_empty() {
            format!("ssh master to {target} failed ({status})")
        } else {
            format!("ssh master to {target} failed ({status}): {said}")
        });
    }
    mark_master_owned(target);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        if sock.exists() {
            return Ok(sock);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("ssh master to {target} did not open {}", sock.display()))
}

/// Targets whose ssh master this process spawned. Other lmux apps, CLI
/// sessions and ssh itself may share a master through its socket, so only
/// these are ever told to exit.
fn owned_masters() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static OWNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    OWNED.get_or_init(Default::default)
}

fn mark_master_owned(target: &str) {
    if let Ok(mut o) = owned_masters().lock() {
        o.insert(target.to_owned());
    }
}

fn master_owned(target: &str) -> bool {
    owned_masters().lock().is_ok_and(|o| o.contains(target))
}

/// Best-effort teardown of a master this process started (app exit). A master
/// that was already running, or that another process started, is left alone.
/// Returns whether an exit was requested.
pub fn close_master(target: &str) -> bool {
    if !master_owned(target) {
        return false;
    }
    if let Ok(mut o) = owned_masters().lock() {
        o.remove(target);
    }
    let sock = control_path(target);
    let _ = Command::new("ssh")
        .args(["-O", "exit"])
        .arg("-o")
        .arg(format!("ControlPath={}", sock.display()))
        .arg("-o")
        .arg("BatchMode=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg(target)
        .status();
    true
}

/// Keep a live master; clear the socket file of a dead one so the next
/// attempt builds a fresh master.
fn refresh_master(target: &str) {
    let sock = control_path(target);
    if sock.exists() && !master_alive(target, &sock) {
        let _ = std::fs::remove_file(&sock);
    }
}

/// Blocking: open the tunnel and wait for the local port. Returns the websocket
/// URL and the (drop-killed) forwarding ssh child. The shared master is left
/// running; it is closed on app exit or expires after `ControlPersist`.
pub fn open_tunnel(host: &HostConfig) -> Result<(String, Option<Tunnel>), String> {
    let target = host.ssh_target.clone().unwrap_or_else(|| host.name.clone());
    let sock = ensure_master(&target)?;
    let port = free_port()?;
    let forward = format!("127.0.0.1:{port}:127.0.0.1:{}", host.remote_port);
    let socks_port = free_port().ok();
    let mut cmd = Command::new("ssh");
    if let Some(s) = socks_port {
        cmd.arg("-D").arg(format!("127.0.0.1:{s}"));
    }
    let mut child = cmd
        .arg("-N")
        .arg("-o")
        .arg(format!("ControlPath={}", sock.display()))
        .arg("-o")
        .arg("ControlMaster=auto")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-L")
        .arg(&forward)
        .arg(&target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // sshd sends the remote host's login banner as session output, which ssh
        // writes to stderr; taken on a pipe it also gives the failure path ssh's
        // own words instead of a bare timeout.
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn ssh: {e}"))?;
    if !wait_port(port, Duration::from_secs(10)) {
        let _ = child.kill();
        run_cancel(cancel_args(&target, &sock, &forward, socks_port));
        let detail = match child.wait_with_output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                }
            }
            Err(_) => String::new(),
        };
        return Err(format!("tunnel to {target} did not open port {port}{detail}"));
    }
    // Drain the banner so a chatty ssh can never block on a full pipe.
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
    }
    Ok((format!("ws://127.0.0.1:{port}"), Some(Tunnel { child: Some(child), port, socks_port, target, sock, forward })))
}

// --- connection ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected,
}

impl ConnState {
    pub fn label(self) -> &'static str {
        match self {
            ConnState::Disconnected => "disconnected",
            ConnState::Connecting => "connecting",
            ConnState::Connected => "connected",
        }
    }
}

/// Why a connection was lost, as far as it decides what to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The network, the tunnel or the daemon is down or slow: retry with backoff.
    Transient,
    /// ssh refused the login. Retrying cannot help until the user acts.
    Auth,
    /// ssh does not trust the host's key.
    HostKey,
    /// The daemon and the app speak different protocol versions.
    Version,
}

impl ErrorClass {
    /// Errors that stop the retries and wait for the user.
    pub fn needs_attention(self) -> bool {
        self != ErrorClass::Transient
    }

    /// The sidebar's one- or two-word status.
    pub fn status(self) -> &'static str {
        match self {
            ErrorClass::Transient => "offline",
            ErrorClass::Auth => "ssh login refused",
            ErrorClass::HostKey => "host key rejected",
            ErrorClass::Version => "needs update",
        }
    }
}

/// Sort an error message (ssh's own words, or ewebsock's) into what to do next.
pub fn classify_error(raw: &str) -> ErrorClass {
    let t = raw.to_lowercase();
    const HOST_KEY: [&str; 4] = [
        "host key verification failed",
        "remote host identification has changed",
        "no matching host key type",
        "offending key",
    ];
    const AUTH: [&str; 4] = [
        "permission denied",
        "too many authentication failures",
        "no supported authentication methods",
        "authentication failed",
    ];
    if HOST_KEY.iter().any(|p| t.contains(p)) {
        ErrorClass::HostKey
    } else if AUTH.iter().any(|p| t.contains(p)) {
        ErrorClass::Auth
    } else {
        ErrorClass::Transient
    }
}

/// A short sentence for the sidebar tooltip and the banner.
pub fn humanize_error(class: ErrorClass, raw: &str) -> String {
    match class {
        ErrorClass::Auth => "ssh login refused (permission denied): check the key or ssh-agent for this host".to_owned(),
        ErrorClass::HostKey => "ssh host key check failed: fix ~/.ssh/known_hosts, then Refresh Connection".to_owned(),
        ErrorClass::Version => raw.to_owned(),
        ErrorClass::Transient => {
            let line = raw.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ");
            match line.char_indices().nth(160) {
                Some((i, _)) => format!("{}…", &line[..i]),
                None if line.is_empty() => "connection failed".to_owned(),
                None => line,
            }
        }
    }
}

/// Retry delays: quick at first (a blip, a wake from sleep), then doubling to
/// a minute so a host that is really gone is not hammered.
const BACKOFF_FIRST: [f64; 4] = [0.3, 0.6, 1.0, 2.0];
const BACKOFF_CAP_SECS: f64 = 60.0;
const BACKOFF_JITTER: f64 = 0.1;
/// A connection must stay healthy this long before the delays start over;
/// one that dies sooner (a daemon that accepts and drops) keeps them growing.
pub const BACKOFF_STABLE: Duration = Duration::from_secs(60);
/// A websocket that has not opened by now is given up on (a stopped daemon
/// still accepts TCP connections, so nothing else would end the wait).
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The whole attempt, tunnel included.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);

/// The delay before retry number `attempt` (0 is the first), without jitter.
pub fn backoff_base(attempt: u32) -> f64 {
    match BACKOFF_FIRST.get(attempt as usize) {
        Some(s) => *s,
        None => (2.0 * 2f64.powi((attempt - 4).min(10) as i32)).min(BACKOFF_CAP_SECS),
    }
}

/// `base` spread by ±10 % with `unit` in `[0, 1]`, never above the cap.
pub fn jittered(base: f64, unit: f64) -> f64 {
    (base * (1.0 + BACKOFF_JITTER * (2.0 * unit.clamp(0.0, 1.0) - 1.0))).min(BACKOFF_CAP_SECS)
}

fn jitter_unit() -> f64 {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    (n % 1000) as f64 / 999.0
}

#[derive(Debug, Default)]
pub struct Backoff {
    attempt: u32,
    /// When the current connection became usable.
    healthy_since: Option<Instant>,
}

impl Backoff {
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Start over (a manual refresh, a host switch).
    pub fn reset(&mut self) {
        *self = Backoff::default();
    }

    /// The connection is usable from `now` (the daemon said hello).
    pub fn healthy(&mut self, now: Instant) {
        self.healthy_since = Some(now);
    }

    /// Forget past failures once the connection has been healthy long enough.
    pub fn settle(&mut self, now: Instant) {
        if self.healthy_since.is_some_and(|s| now.saturating_duration_since(s) >= BACKOFF_STABLE) {
            self.attempt = 0;
        }
    }

    /// The connection is lost at `now`: how long to wait before the next try.
    pub fn next_delay(&mut self, now: Instant, unit: f64) -> Duration {
        self.settle(now);
        self.healthy_since = None;
        let secs = jittered(backoff_base(self.attempt), unit);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_secs_f64(secs)
    }
}

/// Whether an event tagged `event` still belongs to the connection attempt
/// `current`. Every attempt (tunnel resolve plus websocket) has its own
/// number, and anything older is a late answer from one that was given up.
pub fn is_current(event: u64, current: u64) -> bool {
    event == current
}

/// The outcome of a tunnel resolve, tagged with the attempt that asked.
struct Resolved {
    generation: u64,
    result: Result<(String, Option<Tunnel>), String>,
}

pub struct Net {
    pub hosts: Vec<HostConfig>,
    pub host_index: usize,
    pub state: ConnState,
    pub server_version: String,
    pub url: Option<String>,
    /// A short human-readable reason the host is down.
    pub last_error: Option<String>,
    /// Set when retrying cannot help until the user acts (login refused, host
    /// key rejected, version mismatch). Cleared by the next connect attempt.
    pub attention: Option<ErrorClass>,
    backoff: Backoff,
    /// Number of the current connect attempt; see [`is_current`].
    generation: u64,
    /// The attempt the open websocket belongs to.
    ws_generation: u64,
    resolve_tx: mpsc::Sender<Resolved>,
    resolve_rx: Receiver<Resolved>,
    /// When the attempt in flight is given up.
    connect_deadline: Option<Instant>,

    sender: Option<WsSender>,
    receiver: Option<WsReceiver>,
    tunnel: Option<Tunnel>,
    retry_at: Option<Instant>,
    /// Outstanding app-level ping: `(rpc id, sent at)`. `None` when none pending.
    ping: Option<(u64, Instant)>,
    /// When the last ping was sent (also the start of the idle timer).
    last_ping: Instant,
    next_ping_id: u64,
    /// Used to wake the UI thread when a websocket message arrives.
    wake: Option<egui::Context>,
    /// Why the local daemon could not be started, if it could not.
    local_error: Option<String>,
    /// The browser relay or data store for the current host failed.
    browser_net_failed: bool,
    /// `set_network` has run for this host, so `browser_net_failed` is known.
    /// Until then the hello does not claim browsers it may not be able to draw.
    browser_net_checked: bool,
}

impl Drop for Net {
    fn drop(&mut self) {
        if let Some(t) = self.tunnel.as_mut() {
            t.close();
        }
        if let Some(host) = self.current_host() {
            if host.kind == "ssh" {
                let target = host.ssh_target.clone().unwrap_or_else(|| host.name.clone());
                close_master(&target);
            }
        }
    }
}

impl Net {
    pub fn new() -> Self {
        let hosts = load_hosts_with_local();
        // Start on this machine: lmux is a local tool that can also reach out.
        let index = hosts
            .iter()
            .position(|h| h.name == LOCAL_HOST_NAME)
            .unwrap_or_else(|| hosts.iter().position(|h| h.kind != "direct").unwrap_or(0));
        let mut net = Self::with_hosts(hosts);
        net.host_index = index;
        if std::env::var("LMUX_WS_URL").is_err() {
            net.local_error = ensure_local_daemon().err();
        }
        net
    }

    /// One connection to `hosts[index]`. The app holds one of these per host;
    /// each keeps the full list so the create menu can name every host, with
    /// the indices matching the app's own order.
    pub fn for_host(hosts: Vec<HostConfig>, index: usize) -> Self {
        let mut net = Self::with_hosts(hosts);
        net.host_index = index.min(net.hosts.len().saturating_sub(1));
        net
    }

    /// Build a `Net` with explicit hosts (no disk access). Used by tests.
    pub fn with_hosts(hosts: Vec<HostConfig>) -> Self {
        // Start on a real server. The mock is a development stand-in, so it is
        // only picked when no tunneled host is configured; either way the host
        // can be switched from the status bar (or pinned with LMUX_WS_URL).
        let host_index = hosts
            .iter()
            .position(|h| h.kind != "direct")
            .unwrap_or(0);
        let (resolve_tx, resolve_rx) = mpsc::channel();
        Self {
            hosts,
            host_index,
            state: ConnState::Disconnected,
            server_version: String::new(),
            url: None,
            last_error: None,
            attention: None,
            backoff: Backoff::default(),
            generation: 0,
            ws_generation: 0,
            resolve_tx,
            resolve_rx,
            connect_deadline: None,
            sender: None,
            receiver: None,
            tunnel: None,
            retry_at: None,
            ping: None,
            last_ping: Instant::now(),
            next_ping_id: PING_ID_BASE,
            wake: None,
            local_error: None,
            browser_net_failed: false,
            browser_net_checked: false,
        }
    }

    /// Every host the app offers, local first. The sidebar's create menu reads
    /// this to ask which host a new session belongs to.
    pub fn hosts(&self) -> &[HostConfig] {
        &self.hosts
    }

    /// Replace the host list (a host was added or removed) and point this
    /// connection at `index` in it, which must still be the same host.
    pub fn set_hosts(&mut self, hosts: Vec<HostConfig>, index: usize) {
        self.hosts = hosts;
        self.host_index = index;
    }

    pub fn host_index(&self) -> usize {
        self.host_index
    }

    /// Why the local daemon could not be started, if it could not. Taken once,
    /// by the app, to put it in the status banner.
    pub fn take_local_error(&mut self) -> Option<String> {
        self.local_error.take()
    }

    /// Store the egui context so incoming websocket events can request a repaint.
    pub fn set_wake(&mut self, ctx: egui::Context) {
        self.wake = Some(ctx);
    }

    /// Wake the UI thread so a connection-state change is drawn even when the
    /// window is idle. Without this, a tunnel that resolves in a background
    /// thread leaves the app showing "connecting…" until the mouse moves.
    fn wake(&self) {
        if let Some(ctx) = &self.wake {
            ctx.request_repaint();
        }
    }

    pub fn current_host(&self) -> Option<&HostConfig> {
        self.hosts.get(self.host_index)
    }

    pub fn host_name(&self) -> String {
        self.current_host().map(|h| h.label.clone()).unwrap_or_default()
    }

    /// True while the connected host is the machine this app runs on. Terminal
    /// panes there attach through a local PTY instead of the daemon's stream.
    pub fn is_local_host(&self) -> bool {
        self.current_host()
            .is_some_and(|h| h.name == LOCAL_HOST_NAME)
    }

    /// Device scale factor for the browser stream: the display's own
    /// (`pixels_per_point`, 2.0 on Retina), so text is as sharp as in a real
    /// browser. A 1.0 stream looked pixelated once stretched onto a Retina
    /// pane. The daemon's adaptive quality still drops resolution on a slow
    /// link. `LMUX_BROWSER_SCALE` or the host's `browser_scale` overrides it.
    pub fn browser_scale(&self, pixels_per_point: f32) -> f32 {
        if let Ok(v) = std::env::var("LMUX_BROWSER_SCALE") {
            if let Ok(v) = v.parse::<f32>() {
                return v.clamp(1.0, 3.0);
            }
        }
        self.current_host()
            .and_then(|h| h.browser_scale)
            .unwrap_or(pixels_per_point)
            .clamp(1.0, 2.0)
    }

    pub fn select_host(&mut self, index: usize, ctx: &egui::Context) {
        if index >= self.hosts.len() || index == self.host_index {
            return;
        }
        self.tunnel = None;
        self.drop_connection();
        self.host_index = index;
        self.backoff.reset();
        self.retry_at = None;
        self.begin_connect(ctx);
    }

    pub fn begin_connect(&mut self, _ctx: &egui::Context) {
        if matches!(self.state, ConnState::Connecting) {
            return;
        }
        let Some(host) = self.current_host().cloned() else {
            return;
        };
        self.state = ConnState::Connecting;
        self.last_error = None;
        self.attention = None;
        self.retry_at = None;
        self.tunnel = None;
        // Retires a socket that is still open (a reinstall reconnects a live host).
        self.drop_connection();
        self.generation += 1;
        let generation = self.generation;
        self.connect_deadline = Some(Instant::now() + CONNECT_TIMEOUT);

        if host.kind == "direct" {
            let url = host.url.clone().unwrap_or_else(|| MOCK_URL.to_owned());
            self.open_ws(generation, url);
        } else {
            let tx = self.resolve_tx.clone();
            // The tunnel resolves on this thread, so it must wake the UI when it
            // is done: `poll` only runs during a repaint.
            let wake = self.wake.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Resolved { generation, result: open_tunnel(&host) });
                if let Some(ctx) = wake {
                    ctx.request_repaint();
                }
            });
        }
        self.wake();
    }

    fn open_ws(&mut self, generation: u64, url: String) {
        let ctx = self.wake.clone();
        self.url = Some(url.clone());
        match ewebsock::connect_with_wakeup(url, Options::default(), move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        }) {
            Ok((sender, receiver)) => {
                self.sender = Some(sender);
                self.receiver = Some(receiver);
                self.ws_generation = generation;
                self.connect_deadline = Some(Instant::now() + WS_CONNECT_TIMEOUT);
            }
            Err(e) => self.fail(Some(e)),
        }
    }

    fn send_hello(&mut self) {
        let msg = ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: format!("lmux-app/{}", env!("CARGO_PKG_VERSION")),
            draws_browsers: self.draws_browsers(),
        };
        self.send(&msg);
    }

    /// This app runs the host's new browser tabs itself, in WebKit with the
    /// host's network (see `crate::socks`): pages react at local speed and
    /// look as sharp as any Mac browser. Needs an ssh host (for the SOCKS
    /// forward) and macOS 14 (per-host WebKit data stores with a proxy).
    /// `"stream_browsers": true` in hosts.json, or `LMUX_STREAM_BROWSERS=1`,
    /// keeps the host's Chromium and its frame stream instead.
    pub fn draws_browsers(&self) -> bool {
        self.wants_browsers() && self.browser_net_checked && !self.browser_net_failed
    }

    /// Whether this host's pages should run here, before the relay and data
    /// store (which can fail) are known. See [`Net::draws_browsers`].
    pub fn wants_browsers(&self) -> bool {
        if std::env::var("LMUX_STREAM_BROWSERS").is_ok_and(|v| v == "1") {
            return false;
        }
        self.current_host().is_some_and(|h| h.kind == "ssh" && !h.stream_browsers)
            && self.socks_port().is_some()
            && crate::webview::proxies_supported()
    }

    /// Record that the relay or data store for this host could not be set up
    /// (or now can). A connected daemon is told, so it stops giving this app tabs.
    pub fn set_browser_net_failed(&mut self, failed: bool) {
        if self.browser_net_failed != failed || !self.browser_net_checked {
            self.browser_net_failed = failed;
            self.browser_net_checked = true;
            if self.state == ConnState::Connected {
                self.send_hello();
            }
        }
    }

    pub fn is_ssh(&self) -> bool {
        self.current_host().is_some_and(|h| h.kind == "ssh")
    }

    /// The daemon port on the host, which pages must not reach.
    pub fn remote_port(&self) -> u16 {
        self.current_host().map_or(DEFAULT_REMOTE_PORT, |h| h.remote_port)
    }

    /// The host's SOCKS proxy port while its tunnel is up.
    pub fn socks_port(&self) -> Option<u16> {
        self.tunnel.as_ref().and_then(|t| t.socks_port)
    }

    pub fn send(&mut self, msg: &ClientMsg) {
        if let Some(sender) = self.sender.as_mut() {
            sender.send(WsMessage::Text(encode(msg)));
        }
    }

    /// Send `system.ping` and remember the id so the pong can be recognised.
    fn send_ping(&mut self) {
        let id = self.next_ping_id;
        self.next_ping_id += 1;
        let now = Instant::now();
        self.ping = Some((id, now));
        self.last_ping = now;
        let msg = wire::rpc_empty(id, m::PING);
        self.send(&msg);
    }

    /// Forget the websocket. The attempt number moves on, so a tunnel resolve
    /// or socket event still on its way is ignored when it arrives.
    fn drop_connection(&mut self) {
        self.sender = None;
        self.receiver = None;
        self.connect_deadline = None;
        self.generation += 1;
    }

    /// Reconnect now, whatever the state: an attempt in flight is abandoned,
    /// the backoff starts over, and a dead ssh master is dropped so the new
    /// attempt builds a fresh one. The sidebar's "Refresh Connection".
    pub fn reconnect_now(&mut self, ctx: &egui::Context) {
        // Drop this app's tunnel first so its cancel reaches a live master.
        // A live master stays (others may share it); only a dead one's socket goes.
        self.tunnel = None;
        if let Some(host) = self.current_host() {
            if host.kind == "ssh" {
                let target = host.ssh_target.clone().unwrap_or_else(|| host.name.clone());
                refresh_master(&target);
            }
        }
        // A resolve still running keeps its thread but its answer is ignored.
        self.drop_connection();
        self.ping = None;
        self.state = ConnState::Disconnected;
        self.backoff.reset();
        self.retry_at = None;
        self.begin_connect(ctx);
    }

    /// Skip the rest of a backoff wait, as when the window comes back into
    /// focus after the Mac slept or changed network. Does nothing for a host
    /// that waits on the user (see [`Net::attention`]).
    pub fn retry_soon(&mut self) {
        if self.state == ConnState::Disconnected && self.retry_at.is_some() {
            self.retry_at = Some(Instant::now());
            self.wake();
        }
    }

    fn schedule_retry(&mut self) {
        let delay = self.backoff.next_delay(Instant::now(), jitter_unit());
        self.retry_at = Some(Instant::now() + delay);
    }

    /// The connection is lost or could not be made. Transient errors retry
    /// after the backoff; the rest wait for the user.
    fn fail(&mut self, raw: Option<String>) {
        let class = raw.as_deref().map_or(ErrorClass::Transient, classify_error);
        let message = raw.map(|r| humanize_error(class, &r));
        self.fail_with(class, message);
    }

    fn fail_with(&mut self, class: ErrorClass, message: Option<String>) {
        self.drop_connection();
        self.ping = None;
        self.state = ConnState::Disconnected;
        if message.is_some() {
            self.last_error = message;
        }
        if class.needs_attention() {
            self.attention = Some(class);
            self.retry_at = None;
            self.tunnel = None;
        } else {
            self.schedule_retry();
        }
    }

    /// A tunnel resolve finished. A late answer from an abandoned attempt is
    /// dropped, which closes its tunnel.
    fn on_resolved(&mut self, r: Resolved) {
        if !is_current(r.generation, self.generation) {
            return;
        }
        match r.result {
            Ok((url, tunnel)) => {
                self.tunnel = tunnel;
                self.open_ws(r.generation, url);
            }
            Err(e) => self.fail(Some(e)),
        }
    }

    /// One websocket event. Returns the message to hand on, if it is one.
    fn on_ws_event(&mut self, generation: u64, ev: WsEvent) -> Option<WsMessage> {
        if !is_current(generation, self.generation) {
            return None;
        }
        match ev {
            WsEvent::Opened => {
                self.state = ConnState::Connected;
                self.last_error = None;
                self.connect_deadline = None;
                self.ping = None;
                self.last_ping = Instant::now();
                self.send_hello();
                None
            }
            WsEvent::Message(m) => {
                if let WsMessage::Text(t) = &m {
                    match wire::decode_server(t) {
                        Some(ServerMsg::Welcome { protocol_version, .. }) if protocol_version != PROTOCOL_VERSION => {
                            self.version_mismatch(protocol_version);
                            return None;
                        }
                        Some(ServerMsg::Welcome { .. }) => self.backoff.healthy(Instant::now()),
                        Some(ServerMsg::Incompatible { protocol_version, message, .. }) => {
                            self.version_mismatch_msg(protocol_version, Some(message));
                            return None;
                        }
                        // Any matching RpcResult answers the outstanding ping.
                        Some(ServerMsg::RpcResult { response }) => {
                            if self.ping.is_some_and(|(id, _)| id == response.id) {
                                self.ping = None;
                            }
                        }
                        _ => {}
                    }
                }
                Some(m)
            }
            WsEvent::Error(e) => {
                self.fail(Some(e));
                None
            }
            WsEvent::Closed => {
                self.fail(None);
                None
            }
        }
    }

    fn version_mismatch(&mut self, daemon: u32) {
        self.version_mismatch_msg(daemon, None);
    }

    fn version_mismatch_msg(&mut self, daemon: u32, detail: Option<String>) {
        let text = detail.unwrap_or_else(|| format!("lmux on this host speaks protocol v{daemon}, this app v{PROTOCOL_VERSION}"));
        self.fail_with(ErrorClass::Version, Some(format!("needs update: {text}")));
    }

    /// Drive timers/reconnects and return the messages received this frame.
    pub fn poll(&mut self, ctx: &egui::Context) -> Vec<WsMessage> {
        let state_before = self.state;
        let error_before = self.last_error.clone();
        while let Ok(r) = self.resolve_rx.try_recv() {
            self.on_resolved(r);
        }

        let mut out = Vec::new();
        while let Some(ev) = self.receiver.as_ref().and_then(|r| r.try_recv()) {
            if let Some(m) = self.on_ws_event(self.ws_generation, ev) {
                out.push(m);
            }
        }

        if self.state == ConnState::Connecting && self.connect_deadline.is_some_and(|d| Instant::now() >= d) {
            self.fail(Some("connect timed out".to_owned()));
        }
        if self.state == ConnState::Connecting {
            if let Some(d) = self.connect_deadline {
                ctx.request_repaint_after(d.saturating_duration_since(Instant::now()));
            }
        }

        // Reconnect once the backoff has elapsed.
        if self.state == ConnState::Disconnected {
            match self.retry_at {
                Some(t) if Instant::now() >= t => {
                    self.retry_at = None;
                    self.begin_connect(ctx);
                }
                Some(t) => {
                    ctx.request_repaint_after(t.saturating_duration_since(Instant::now()));
                }
                None => {}
            }
        }

        // App-level liveness: ping on an idle timer and give up early if the
        // pong never arrives (review #14).
        if self.state == ConnState::Connected {
            let now = Instant::now();
            self.backoff.settle(now);
            if let Some((_, sent)) = self.ping {
                if now.duration_since(sent) >= PING_TIMEOUT {
                    self.fail(Some(format!("link timed out (no ping reply in {}s)", PING_TIMEOUT.as_secs())));
                }
            }
            if self.state == ConnState::Connected {
                if self.ping.is_none() && now.duration_since(self.last_ping) >= PING_INTERVAL {
                    self.send_ping();
                }
                let due = match self.ping {
                    Some((_, sent)) => PING_TIMEOUT.saturating_sub(now.duration_since(sent)),
                    None => PING_INTERVAL.saturating_sub(now.duration_since(self.last_ping)),
                };
                // Keep the timer running even when the window is otherwise idle.
                ctx.request_repaint_after(due.max(Duration::from_millis(50)));
            }
        }

        // A state change handled here (opened/closed/error/tunnel result) must
        // repaint even if ewebsock's own wakeup never fired for it.
        if self.state != state_before || self.last_error != error_before {
            self.wake();
        }
        out
    }

    /// How many retries in a row have failed since the connection was last
    /// healthy for a stable period.
    pub fn attempt(&self) -> u32 {
        self.backoff.attempt()
    }

    /// The daemon on this host speaks another protocol version than the app.
    pub fn needs_update(&self) -> bool {
        self.attention == Some(ErrorClass::Version)
    }
}

#[cfg(test)]
mod tunnel_cancel_tests {
    use super::*;

    #[test]
    fn only_masters_this_process_started_are_exited() {
        let t = "lmux-test-no-such-host-owned";
        assert!(!close_master(t), "a master we did not start must be left alone");
        mark_master_owned(t);
        assert!(close_master(t));
        assert!(!close_master(t), "ownership ends once the master is told to exit");
    }

    #[test]
    fn refresh_keeps_a_missing_socket_and_clears_a_dead_one() {
        let t = "lmux-test-no-such-host-dead";
        let sock = control_path(t);
        let _ = std::fs::remove_file(&sock);
        refresh_master(t);
        assert!(!sock.exists());
        std::fs::write(&sock, b"").unwrap();
        refresh_master(t);
        assert!(!sock.exists(), "a socket file with no master behind it is removed");
    }

    #[test]
    fn cancel_names_both_forwards_and_the_master() {
        let a = cancel_args("box", std::path::Path::new("/tmp/lmux-ssh-1"), "127.0.0.1:5000:127.0.0.1:7878", Some(6000));
        assert_eq!(
            a,
            ["-O", "cancel", "-L", "127.0.0.1:5000:127.0.0.1:7878", "-D", "127.0.0.1:6000", "-o", "ControlPath=/tmp/lmux-ssh-1", "-o", "BatchMode=yes", "box"]
        );
        let a = cancel_args("box", std::path::Path::new("/s"), "f", None);
        assert!(!a.contains(&"-D".to_owned()));
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    fn direct() -> HostConfig {
        HostConfig {
            name: "t".into(),
            label: "t".into(),
            kind: "direct".into(),
            url: Some("ws://127.0.0.1:9".into()),
            ssh_target: None,
            remote_port: DEFAULT_REMOTE_PORT,
            browser_scale: None,
            stream_browsers: false,
            cloud: None,
        }
    }

    fn net() -> Net {
        Net::with_hosts(vec![direct()])
    }

    #[test]
    fn schedule_is_fast_first_then_doubles_to_the_cap() {
        let got: Vec<f64> = (0..12).map(backoff_base).collect();
        assert_eq!(got, [0.3, 0.6, 1.0, 2.0, 2.0, 4.0, 8.0, 16.0, 32.0, 60.0, 60.0, 60.0]);
        assert_eq!(backoff_base(u32::MAX), 60.0);
    }

    #[test]
    fn jitter_stays_within_ten_percent_and_under_the_cap() {
        assert_eq!(jittered(10.0, 0.5), 10.0);
        assert!((jittered(10.0, 0.0) - 9.0).abs() < 1e-9);
        assert!((jittered(10.0, 1.0) - 11.0).abs() < 1e-9);
        assert_eq!(jittered(60.0, 1.0), 60.0);
        assert_eq!(jittered(10.0, 7.0), jittered(10.0, 1.0));
    }

    #[test]
    fn delays_keep_growing_after_brief_connects() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut delays = Vec::new();
        for i in 0..6u32 {
            // Connected for a moment each time, then lost again.
            let up = t0 + Duration::from_secs(u64::from(i) * 10);
            b.healthy(up);
            delays.push(b.next_delay(up + Duration::from_secs(5), 0.5).as_secs_f64());
        }
        assert_eq!(delays, [0.3, 0.6, 1.0, 2.0, 2.0, 4.0]);
        assert_eq!(b.attempt(), 6);
    }

    #[test]
    fn a_stable_connection_resets_the_delays() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        for _ in 0..5 {
            b.next_delay(t0, 0.5);
        }
        b.healthy(t0);
        b.settle(t0 + BACKOFF_STABLE - Duration::from_secs(1));
        assert_eq!(b.attempt(), 5, "not stable yet");
        b.settle(t0 + BACKOFF_STABLE);
        assert_eq!(b.attempt(), 0);
        // The loss itself also settles first.
        let mut b = Backoff::default();
        for _ in 0..5 {
            b.next_delay(t0, 0.5);
        }
        b.healthy(t0);
        let d = b.next_delay(t0 + BACKOFF_STABLE + Duration::from_secs(1), 0.5);
        assert_eq!(d, Duration::from_secs_f64(0.3));
    }

    #[test]
    fn a_failure_without_a_healthy_connection_never_resets() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        for _ in 0..3 {
            b.next_delay(t0, 0.5);
        }
        b.settle(t0 + Duration::from_secs(3600));
        assert_eq!(b.attempt(), 3);
    }

    #[test]
    fn manual_reset_starts_over() {
        let mut b = Backoff::default();
        b.next_delay(Instant::now(), 0.5);
        b.reset();
        assert_eq!(b.attempt(), 0);
    }

    #[test]
    fn only_the_current_generation_is_accepted() {
        assert!(is_current(4, 4));
        assert!(!is_current(3, 4));
        assert!(!is_current(5, 4));
    }

    #[test]
    fn a_stale_resolve_is_ignored() {
        let mut n = net();
        n.generation = 7;
        n.state = ConnState::Connecting;
        n.on_resolved(Resolved { generation: 6, result: Err("old failure".into()) });
        assert_eq!(n.state, ConnState::Connecting);
        assert!(n.last_error.is_none() && n.retry_at.is_none());
        n.on_resolved(Resolved { generation: 7, result: Err("tunnel to box did not open port 1".into()) });
        assert_eq!(n.state, ConnState::Disconnected);
        assert!(n.last_error.as_deref().unwrap().contains("did not open"));
        assert!(n.retry_at.is_some());
    }

    #[test]
    fn stale_socket_events_do_not_touch_the_current_connection() {
        let mut n = net();
        n.generation = 3;
        n.state = ConnState::Connected;
        assert!(n.on_ws_event(2, WsEvent::Closed).is_none());
        assert!(n.on_ws_event(2, WsEvent::Error("boom".into())).is_none());
        assert!(n.on_ws_event(2, WsEvent::Message(WsMessage::Text("{}".into()))).is_none());
        assert_eq!(n.state, ConnState::Connected);
        assert!(n.last_error.is_none());
        assert_eq!(n.generation, 3, "a stale event must not supersede the current attempt");
        n.on_ws_event(3, WsEvent::Closed);
        assert_eq!(n.state, ConnState::Disconnected);
        assert_eq!(n.generation, 4, "losing a connection retires it");
    }

    #[test]
    fn dropping_a_connection_retires_events_in_flight() {
        let mut n = net();
        n.generation = 1;
        n.ws_generation = 1;
        n.drop_connection();
        assert!(!is_current(n.ws_generation, n.generation));
    }

    #[test]
    fn a_transient_error_retries() {
        let mut n = net();
        n.generation = 1;
        n.state = ConnState::Connected;
        n.on_ws_event(1, WsEvent::Error("connection refused".into()));
        assert_eq!(n.state, ConnState::Disconnected);
        assert!(n.retry_at.is_some());
        assert_eq!(n.attention, None);
        assert_eq!(n.attempt(), 1);
    }

    #[test]
    fn a_newer_protocol_stops_the_retries() {
        let mut n = net();
        n.generation = 1;
        n.state = ConnState::Connected;
        let welcome = format!(
            r#"{{"type":"welcome","protocol_version":{},"server_version":"9.9","host":"h"}}"#,
            PROTOCOL_VERSION + 1
        );
        assert!(n.on_ws_event(1, WsEvent::Message(WsMessage::Text(welcome))).is_none());
        assert_eq!(n.state, ConnState::Disconnected);
        assert_eq!(n.attention, Some(ErrorClass::Version));
        assert!(n.needs_update());
        assert!(n.retry_at.is_none(), "no retry loop");
        assert!(n.last_error.as_deref().unwrap().starts_with("needs update"));
        n.retry_soon();
        assert!(n.retry_at.is_none());
    }

    #[test]
    fn an_incompatible_reply_stops_the_retries() {
        let mut n = net();
        n.generation = 1;
        n.state = ConnState::Connected;
        let msg = r#"{"type":"incompatible","protocol_version":3,"server_version":"2.0","message":"daemon speaks v3"}"#;
        n.on_ws_event(1, WsEvent::Message(WsMessage::Text(msg.into())));
        assert_eq!(n.attention, Some(ErrorClass::Version));
        assert!(n.last_error.as_deref().unwrap().contains("daemon speaks v3"));
        assert!(n.retry_at.is_none());
    }

    #[test]
    fn a_matching_welcome_is_passed_on() {
        let mut n = net();
        n.generation = 1;
        n.state = ConnState::Connected;
        let welcome = format!(r#"{{"type":"welcome","protocol_version":{PROTOCOL_VERSION},"server_version":"1","host":"h"}}"#);
        assert!(n.on_ws_event(1, WsEvent::Message(WsMessage::Text(welcome))).is_some());
        assert_eq!(n.state, ConnState::Connected);
        assert_eq!(n.attention, None);
    }

    #[test]
    fn errors_are_classified() {
        use ErrorClass::*;
        let c = classify_error;
        assert_eq!(c("ssh master to box failed (exit status: 255): user@box: Permission denied (publickey)."), Auth);
        assert_eq!(c("Received disconnect: Too many authentication failures"), Auth);
        assert_eq!(c("Host key verification failed."), HostKey);
        assert_eq!(
            c("@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\nHost key verification failed."),
            HostKey
        );
        assert_eq!(c("ssh: connect to host box port 22: Connection refused"), Transient);
        assert_eq!(c("ssh: Could not resolve hostname box"), Transient);
        assert_eq!(c("tunnel to box did not open port 5000"), Transient);
        assert_eq!(c("link timed out (no ping reply in 3s)"), Transient);
        assert_eq!(c(""), Transient);
        assert!(Auth.needs_attention() && HostKey.needs_attention() && Version.needs_attention());
        assert!(!Transient.needs_attention());
    }

    #[test]
    fn an_auth_failure_waits_for_the_user() {
        let mut n = net();
        n.generation = 1;
        n.state = ConnState::Connecting;
        n.on_resolved(Resolved {
            generation: 1,
            result: Err("ssh master to box failed (exit status: 255): box: Permission denied (publickey).".into()),
        });
        assert_eq!(n.attention, Some(ErrorClass::Auth));
        assert!(n.retry_at.is_none());
        assert!(n.last_error.as_deref().unwrap().contains("login refused"));
    }

    #[test]
    fn humanized_errors_are_one_short_line() {
        let long = format!("first\n\n{}", "x".repeat(400));
        let h = humanize_error(ErrorClass::Transient, &long);
        assert!(!h.contains('\n'));
        assert!(h.chars().count() <= 161);
        assert_eq!(humanize_error(ErrorClass::Transient, "  "), "connection failed");
    }
}
