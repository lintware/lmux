//! Transport: host config, SSH tunnel and the websocket client with backoff.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
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

pub fn default_hosts() -> Vec<HostConfig> {
    vec![
        HostConfig {
            name: "my-server".into(),
            label: "my-server".into(),
            kind: "ssh".into(),
            url: None,
            ssh_target: Some("my-server".into()),
            remote_port: DEFAULT_REMOTE_PORT,
            browser_scale: None,
            cloud: None,
        },
        HostConfig {
            name: "mock".into(),
            label: "mock (local)".into(),
            kind: "direct".into(),
            url: Some(MOCK_URL.into()),
            ssh_target: None,
            remote_port: DEFAULT_REMOTE_PORT,
            browser_scale: None,
            cloud: None,
        },
    ]
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
    child: Child,
    pub port: u16,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
fn control_path(target: &str) -> PathBuf {
    let mut h = DefaultHasher::new();
    target.hash(&mut h);
    std::env::temp_dir().join(format!("lmux-ssh-{:016x}", h.finish()))
}

/// Make sure a background SSH master exists for `target`. `ControlPersist`
/// keeps the transport alive between tunnels, so the next `open_tunnel` skips
/// the handshake and returns almost instantly.
fn ensure_master(target: &str) -> Result<PathBuf, String> {
    let sock = control_path(target);
    if sock.exists() {
        return Ok(sock);
    }
    let status = Command::new("ssh")
        .args(["-fN", "-M"])
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
        .stderr(Stdio::null())
        .arg(target)
        .status()
        .map_err(|e| format!("spawn ssh master: {e}"))?;
    if !status.success() {
        return Err(format!("ssh master to {target} failed ({status})"));
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        if sock.exists() {
            return Ok(sock);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("ssh master to {target} did not open {}", sock.display()))
}

/// Best-effort teardown of a master started by [`ensure_master`] (app exit).
pub fn close_master(target: &str) {
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
}

/// Blocking: open the tunnel and wait for the local port. Returns the websocket
/// URL and the (drop-killed) forwarding ssh child. The shared master is left
/// running; it is closed on app exit or expires after `ControlPersist`.
pub fn open_tunnel(host: &HostConfig) -> Result<(String, Option<Tunnel>), String> {
    let target = host.ssh_target.clone().unwrap_or_else(|| host.name.clone());
    let sock = ensure_master(&target)?;
    let port = free_port()?;
    let forward = format!("127.0.0.1:{port}:127.0.0.1:{}", host.remote_port);
    let mut child = Command::new("ssh")
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
    Ok((format!("ws://127.0.0.1:{port}"), Some(Tunnel { child, port })))
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

enum Resolve {
    Idle,
    Pending(Receiver<Result<(String, Option<Tunnel>), String>>),
}

pub struct Net {
    pub hosts: Vec<HostConfig>,
    pub host_index: usize,
    pub state: ConnState,
    pub server_version: String,
    pub url: Option<String>,
    pub last_error: Option<String>,
    pub attempt: u32,

    sender: Option<WsSender>,
    receiver: Option<WsReceiver>,
    tunnel: Option<Tunnel>,
    resolve: Resolve,
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
}

impl Drop for Net {
    fn drop(&mut self) {
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
        Self {
            hosts,
            host_index,
            state: ConnState::Disconnected,
            server_version: String::new(),
            url: None,
            last_error: None,
            attempt: 0,
            sender: None,
            receiver: None,
            tunnel: None,
            resolve: Resolve::Idle,
            retry_at: None,
            ping: None,
            last_ping: Instant::now(),
            next_ping_id: PING_ID_BASE,
            wake: None,
            local_error: None,
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

    /// Device scale factor for the browser stream. Defaults to 1.0 over the
    /// network (a Retina 2.0 would cost 4× the pixels); `LMUX_BROWSER_SCALE`
    /// or the host's `browser_scale` opts into more (review #9).
    pub fn browser_scale(&self) -> f32 {
        if let Ok(v) = std::env::var("LMUX_BROWSER_SCALE") {
            if let Ok(v) = v.parse::<f32>() {
                return v.clamp(1.0, 3.0);
            }
        }
        self.current_host()
            .and_then(|h| h.browser_scale)
            .unwrap_or(1.0)
            .clamp(1.0, 3.0)
    }

    pub fn select_host(&mut self, index: usize, ctx: &egui::Context) {
        if index >= self.hosts.len() || index == self.host_index {
            return;
        }
        self.tunnel = None;
        self.drop_connection();
        self.host_index = index;
        self.attempt = 0;
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
        self.retry_at = None;
        self.tunnel = None;

        if host.kind == "direct" {
            let url = host.url.clone().unwrap_or_else(|| MOCK_URL.to_owned());
            self.open_ws(url);
        } else {
            let (tx, rx) = mpsc::channel();
            self.resolve = Resolve::Pending(rx);
            // The tunnel resolves on this thread, so it must wake the UI when it
            // is done: `poll` only runs during a repaint.
            let wake = self.wake.clone();
            std::thread::spawn(move || {
                let _ = tx.send(open_tunnel(&host));
                if let Some(ctx) = wake {
                    ctx.request_repaint();
                }
            });
        }
        self.wake();
    }

    fn open_ws(&mut self, url: String) {
        let ctx = self.wake.clone();
        match ewebsock::connect_with_wakeup(url.clone(), Options::default(), move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        }) {
            Ok((sender, receiver)) => {
                self.sender = Some(sender);
                self.receiver = Some(receiver);
                self.url = Some(url);
            }
            Err(e) => {
                self.last_error = Some(e);
                self.state = ConnState::Disconnected;
                self.url = Some(url);
                self.schedule_retry();
            }
        }
    }

    fn send_hello(&mut self) {
        let msg = ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: format!("lmux-app/{}", env!("CARGO_PKG_VERSION")),
        };
        self.send(&msg);
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

    fn drop_connection(&mut self) {
        self.sender = None;
        self.receiver = None;
    }

    fn schedule_retry(&mut self) {
        let secs = [0.5f64, 1.0, 2.0, 4.0, 8.0, 15.0][(self.attempt as usize).min(5)];
        self.attempt = self.attempt.saturating_add(1);
        self.retry_at = Some(Instant::now() + Duration::from_secs_f64(secs));
    }

    /// Drive timers/reconnects and return the messages received this frame.
    pub fn poll(&mut self, ctx: &egui::Context) -> Vec<WsMessage> {
        let state_before = self.state;
        let error_before = self.last_error.clone();
        // Tunnel resolution finished?
        if let Resolve::Pending(rx) = &self.resolve {
            match rx.try_recv() {
                Ok(Ok((url, tunnel))) => {
                    self.tunnel = tunnel;
                    self.open_ws(url);
                    self.resolve = Resolve::Idle;
                }
                Ok(Err(e)) => {
                    self.last_error = Some(e);
                    self.state = ConnState::Disconnected;
                    self.schedule_retry();
                    self.resolve = Resolve::Idle;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.resolve = Resolve::Idle;
                }
            }
        }

        let mut out = Vec::new();
        while let Some(ev) = self.receiver.as_ref().and_then(|r| r.try_recv()) {
            match ev {
                WsEvent::Opened => {
                    self.state = ConnState::Connected;
                    self.attempt = 0;
                    self.last_error = None;
                    self.ping = None;
                    self.last_ping = Instant::now();
                    self.send_hello();
                }
                WsEvent::Message(m) => {
                    // Any matching RpcResult answers the outstanding ping.
                    if let WsMessage::Text(t) = &m {
                        if let Some((id, _)) = self.ping {
                            if let Some(ServerMsg::RpcResult { response }) = wire::decode_server(t) {
                                if response.id == id {
                                    self.ping = None;
                                }
                            }
                        }
                    }
                    out.push(m)
                }
                WsEvent::Error(e) => {
                    self.last_error = Some(e);
                    self.drop_connection();
                    self.state = ConnState::Disconnected;
                    self.schedule_retry();
                }
                WsEvent::Closed => {
                    self.drop_connection();
                    self.state = ConnState::Disconnected;
                    self.schedule_retry();
                }
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
            if let Some((_, sent)) = self.ping {
                if now.duration_since(sent) >= PING_TIMEOUT {
                    self.last_error = Some(format!("link timed out (no ping reply in {}s)", PING_TIMEOUT.as_secs()));
                    self.ping = None;
                    self.drop_connection();
                    self.state = ConnState::Disconnected;
                    self.schedule_retry();
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
}
