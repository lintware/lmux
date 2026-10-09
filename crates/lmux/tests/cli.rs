//! End to end: the `lmux` binary against a scratch daemon on its own tmux
//! socket. Skipped when tmux is not installed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_lmux");

struct Daemon {
    child: Child,
    socket: PathBuf,
    tmux_socket: String,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("tmux")
            .args(["-L", &self.tmux_socket, "kill-server"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

impl Daemon {
    fn start() -> Option<Daemon> {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return None;
        }
        // Short path: unix socket paths are limited to ~100 bytes on macOS.
        let dir = tempfile::Builder::new().prefix("lmuxcli").tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("s");
        let tmux_socket = format!("lmux-cli-it-{}", std::process::id());
        let child = Command::new(BIN)
            .arg("--socket")
            .arg(&socket)
            .arg("server")
            .arg("--state-dir")
            .arg(dir.path().join("state"))
            .args(["--tmux-socket", &tmux_socket, "--ws-addr", "127.0.0.1:0", "--no-browser"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lmux server");
        let d = Daemon { child, socket, tmux_socket, _dir: dir };
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if d.run(&["ping"]).status.success() {
                return Some(d);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("daemon did not answer ping");
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.arg("--socket").arg(&self.socket).args(args);
        c.env_remove("LMUX_SOCKET").env_remove("LMUX_WORKSPACE_ID").env_remove("LMUX_SURFACE_ID");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("run lmux")
    }

    /// Run, require success, return stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "lmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        serde_json::from_str(&self.ok(&a)).expect("json output")
    }
}

fn cwd(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn cli_parity_commands_against_a_scratch_daemon() {
    let Some(d) = Daemon::start() else { return };
    let work = tempfile::tempdir().unwrap();
    let work = cwd(work.path());

    // A workspace whose terminal runs a plain sh, so rc files don't matter.
    let ws = d.json(&["new-workspace", "--name", "a", "--cwd", &work, "--command", "/bin/sh"]);
    assert_eq!(ws["workspace"], "workspace:1");
    assert_eq!(d.ok(&["current-workspace"]).trim(), "workspace:1");
    let tab = d.json(&["new-surface", "--pane", "pane:1", "--command", "/bin/sh", "--focus", "false"]);
    let tab = tab["surface"].as_str().unwrap().to_string();

    // list-pane-surfaces; --focus false kept surface:1 selected.
    let listed = d.ok(&["list-pane-surfaces", "--pane", "pane:1"]);
    assert!(listed.contains("* surface:1"), "{listed}");
    assert!(listed.contains(&format!("  {tab}")), "{listed}");

    // send unescapes \n; flags may follow the text; wait-for sees the output.
    d.ok(&["send", "echo", "MARK_$((6*7))\\n", "--surface", "surface:1"]);
    d.ok(&["wait-for", "--text", "MARK_42", "--surface", "surface:1", "--timeout", "10"]);
    let timed_out = d.run(&["wait-for", "--text", "NEVER_THERE", "--surface", "surface:1", "--timeout", "0.5"]);
    assert!(!timed_out.status.success());

    // capture-pane with tmux flags, and read-screen --lines.
    let cap = d.ok(&["capture-pane", "-p", "-t", "surface:1", "-S", "-", "-E", "-"]);
    assert!(cap.contains("MARK_42"), "{cap}");
    let bad = d.run(&["capture-pane", "-t", "surface:1", "-S", "1;kill-server"]);
    assert!(!bad.status.success(), "bad -S values are refused");
    let last = d.ok(&["read-screen", "--surface", "surface:1", "--lines", "2"]);
    assert_eq!(last.trim_end().lines().count(), 2, "{last:?}");
    assert!(last.contains("MARK_42"), "{last:?}");

    // clear-history leaves at most the visible screen.
    d.ok(&["send", "--surface", "surface:1", "i=0; while [ $i -lt 300 ]; do echo L$i; i=$((i+1)); done\\n"]);
    d.ok(&["wait-for", "--text", "L299", "--surface", "surface:1", "--timeout", "10"]);
    d.ok(&["clear-history", "--surface", "surface:1"]);
    let after = d.ok(&["read-screen", "--surface", "surface:1", "--scrollback"]);
    assert!(after.lines().count() < 100, "history cleared: {} lines", after.lines().count());

    // rename-tab, then move it into a new workspace; the moved tab's stale
    // LMUX_WORKSPACE_ID no longer decides its workspace.
    d.ok(&["rename-tab", "--surface", &tab, "build", "logs"]);
    let moved = d.json(&["move-tab-to-new-workspace", "--surface", &tab]);
    let ws2 = moved["workspace"].as_str().unwrap().to_string();
    let tree = d.json(&["tree"]);
    let w2 = tree["workspaces"].as_array().unwrap().iter().find(|w| w["id"] == ws2.as_str()).unwrap();
    assert_eq!(w2["title"], "build logs");
    let out = d
        .cmd(&["current-workspace"])
        .env("LMUX_WORKSPACE_ID", "workspace:1")
        .env("LMUX_SURFACE_ID", &tab)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), ws2);

    // Notifications: jump-to-unread selects the newest unread one's workspace.
    d.ok(&["notify", "--title", "first", "--workspace", "workspace:1"]);
    d.ok(&["notify", "--title", "second", "--workspace", &ws2, "--surface", &tab]);
    d.ok(&["select-workspace", "--workspace", "workspace:1"]);
    assert_eq!(d.ok(&["jump-to-unread"]).trim(), format!("OK {ws2} {tab}"));
    assert_eq!(d.ok(&["current-workspace"]).trim(), ws2);
    let list = d.ok(&["list-notifications"]);
    assert!(list.contains("|read|second"), "{list}");
    d.ok(&["dismiss-notification", "--all-read"]);
    assert!(!d.ok(&["list-notifications"]).contains("second"));
    assert!(!d.run(&["dismiss-notification", "--id", "nope"]).status.success());

    // Status and log.
    d.ok(&["set-status", "build", "ok", "--icon", "hammer", "--workspace", "workspace:1"]);
    assert_eq!(d.ok(&["list-status", "--workspace", "workspace:1"]).trim(), "build=ok icon=hammer");
    d.ok(&["log", "--source", "ci", "--", "-x", "done"]);
    assert_eq!(d.ok(&["list-log"]).trim(), "[ci] [info] -x done");
    d.ok(&["clear-log"]);
    assert_eq!(d.ok(&["list-log"]).trim(), "No log entries");

    // The raw escape hatch.
    let v: serde_json::Value = serde_json::from_str(&d.ok(&["rpc", "system.ping"])).unwrap();
    assert_eq!(v["pong"], true);
    let v: serde_json::Value =
        serde_json::from_str(&d.ok(&["rpc", "pane.surfaces", r#"{"pane":"pane:1"}"#])).unwrap();
    assert_eq!(v["surfaces"].as_array().unwrap().len(), 1);
    assert!(!d.run(&["rpc", "no.such_method"]).status.success());

    // Named signals.
    let name = format!("lmux-it-{}", std::process::id());
    d.ok(&["wait-for", "-S", &name]);
    d.ok(&["wait-for", &name, "--timeout", "2"]);
    assert!(!d.run(&["wait-for", &name, "--timeout", "0.2"]).status.success());

    // Without a browser backend, browser commands say so.
    let b = d.run(&["browser", "surface:1", "snapshot"]);
    assert!(String::from_utf8_lossy(&b.stderr).contains("Unsupported"));
}
