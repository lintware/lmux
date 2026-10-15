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
        // tmux leaves the socket file behind; remove it with the server.
        let path = Command::new("tmux")
            .args(["-L", &self.tmux_socket, "display-message", "-p", "#{socket_path}"])
            .stderr(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|p| !p.is_empty());
        let _ = Command::new("tmux")
            .args(["-L", &self.tmux_socket, "kill-server"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(path) = path {
            let _ = std::fs::remove_file(path);
        }
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
        // One tmux server per daemon: tests in this file run in parallel.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmux_socket = format!("lmux-cli-it-{}-{n}", std::process::id());
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

/// `import-cmux --from <export>` without --move: the workspace is built whole
/// (folder, split and ratio, tabs, directories, a re-attached command, the
/// Claude session tag) and nothing is typed into it; a workspace on a host
/// without lmux is skipped. No cmux is needed for this.
#[test]
fn import_cmux_builds_workspaces_from_an_export() {
    let Some(d) = Daemon::start() else { return };
    let work = tempfile::tempdir().unwrap();
    let work = cwd(work.path());
    let export = serde_json::json!({"windows": [{"tabManager": {
        "workspaceGroups": [{"anchorWorkspaceId": "W1", "name": "Clients"}],
        "workspaces": [
            {"workspaceId": "W1", "customTitle": "imported", "currentDirectory": work,
             "focusedPanelId": "c",
             "layout": {"type": "split", "split": {"orientation": "horizontal", "dividerPosition": 0.66,
                "first": {"type": "pane", "pane": {"panelIds": ["a", "b"], "selectedPanelId": "b"}},
                "second": {"type": "pane", "pane": {"panelIds": ["c"]}}}},
             "panels": [
                {"id": "a", "type": "terminal", "directory": work,
                 "terminal": {"resumeBinding": {"kind": "claude", "checkpointId": "abc-123",
                    "permissionMode": "bypassPermissions", "launchCommand": {"arguments": ["claude"]}}}},
                {"id": "b", "type": "terminal", "directory": work},
                {"id": "c", "type": "terminal", "terminal": {"resumeBinding": {"kind": "tmux",
                    "command": "sh -c 'echo REATTACHED; exec sh'",
                    "launchCommand": {"arguments": ["tmux", "attach", "-t", "x"]}}}}]},
            {"workspaceId": "W2", "customTitle": "far", "remote": {"destination": "lmux-no-such-host.invalid"},
             "layout": {"type": "pane", "pane": {"panelIds": ["d"]}},
             "panels": [{"id": "d", "type": "terminal"}]}
        ]}}]});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), export.to_string()).unwrap();

    let out = d.json(&["import-cmux", "--from", &file.path().to_string_lossy()]);
    let rows = out["workspaces"].as_array().unwrap();
    assert_eq!(rows[0]["status"], "copied", "{out}");
    assert_eq!((rows[0]["tabs"].as_u64(), rows[0]["agents"].as_u64()), (Some(3), Some(1)));
    assert_eq!(rows[1]["status"], "skipped", "{out}");

    let tree = d.json(&["tree"]);
    let ws = tree["workspaces"].as_array().unwrap().iter().find(|w| w["title"] == "imported").unwrap();
    let folder = tree["folders"].as_array().unwrap().iter().find(|f| f["name"] == "Clients").unwrap();
    assert_eq!(ws["folder"], folder["id"]);
    assert_eq!(ws["cwd"], work.as_str());
    let layout = &ws["layout"];
    assert_eq!(layout["direction"], "horizontal");
    assert!((layout["ratio"].as_f64().unwrap() - 0.66).abs() < 1e-3, "{layout}");
    let panes = ws["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 2);
    let first = &panes[0];
    assert_eq!(first["surfaces"].as_array().unwrap().len(), 2);
    // cmux's selected tab and focused pane carry over.
    assert_eq!(first["active_surface"], first["surfaces"][1]["id"]);
    assert_eq!(ws["focused_pane"], panes[1]["id"]);

    // The re-attached tab runs its command; the Claude tab got no input.
    let reattached = panes[1]["surfaces"][0]["id"].as_str().unwrap();
    d.ok(&["wait-for", "--text", "REATTACHED", "--surface", reattached, "--timeout", "10"]);
    let claude_tab = first["surfaces"][0]["id"].as_str().unwrap();
    let screen = d.ok(&["read-screen", "--surface", claude_tab]);
    assert!(!screen.contains("claude --resume"), "{screen}");

    // A second import puts the next workspace in the same folder.
    d.json(&["import-cmux", "--from", &file.path().to_string_lossy(), "--workspace", "W1"]);
    let tree = d.json(&["tree"]);
    let clients = tree["folders"].as_array().unwrap().iter().filter(|f| f["name"] == "Clients").count();
    assert_eq!(clients, 1);
}

/// `lmux share --no-tunnel`: the code-for-cookie exchange, the Origin and
/// cookie checks on the WebSocket, the session list, a live screen, and the
/// view-only default.
#[tokio::test(flavor = "multi_thread")]
async fn share_serves_sessions_with_the_security_rules() {
    use futures_util::{SinkExt, StreamExt};
    use std::io::{BufRead, BufReader, Read, Write};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

    let Some(d) = Daemon::start() else { return };
    d.ok(&["new-workspace", "--name", "shared", "--command", "/bin/sh"]);
    d.ok(&["send", "echo", "SHARE_$((6*7))\\n", "--surface", "surface:1"]);
    d.ok(&["wait-for", "--text", "SHARE_42", "--surface", "surface:1", "--timeout", "10"]);

    // View-only share; stdin stays open (closing it would end the share).
    let mut share = d.cmd(&["--json", "share", "--no-tunnel"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(share.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let info: serde_json::Value = serde_json::from_str(&line).expect("share prints JSON");
    let url = info["url"].as_str().unwrap().to_string();
    assert_eq!(info["typing"], false);
    let (base, code) = url.split_once("/#").unwrap();
    let addr = base.trim_start_matches("http://").to_string();

    let http = |req: String| -> String {
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    };
    let post = |origin: &str, code: &str| {
        let body = format!("{{\"code\":\"{code}\"}}");
        http(format!(
            "POST /auth HTTP/1.1\r\nHost: {addr}\r\nOrigin: {origin}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
    };

    // The page carries a strict CSP and no cookie yet.
    let page = http(format!("GET / HTTP/1.1\r\nHost: {addr}\r\n\r\n"));
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.to_lowercase().contains("content-security-policy: default-src 'none'"), "{page}");

    // A wrong code and a foreign origin are refused and do not spend the code.
    assert!(post(base, "nope").starts_with("HTTP/1.1 401"));
    assert!(post("http://evil.example", code).starts_with("HTTP/1.1 403"));
    let ok = post(base, code);
    assert!(ok.starts_with("HTTP/1.1 204"), "{ok}");
    let cookie_line = ok.lines().find(|l| l.starts_with("Set-Cookie:")).expect("cookie");
    for flag in ["HttpOnly", "Secure", "SameSite=Strict"] {
        assert!(cookie_line.contains(flag), "{cookie_line}");
    }
    let cookie = cookie_line.trim_start_matches("Set-Cookie: ").split(';').next().unwrap().to_string();
    // Single use.
    assert!(post(base, code).starts_with("HTTP/1.1 401"));

    let connect = |origin: Option<&str>, cookie: Option<&str>| {
        let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
        if let Some(o) = origin {
            req.headers_mut().insert("Origin", o.parse().unwrap());
        }
        if let Some(c) = cookie {
            req.headers_mut().insert("Cookie", c.parse().unwrap());
        }
        tokio_tungstenite::connect_async(req)
    };
    assert!(connect(Some(base), None).await.is_err(), "no cookie must be refused");
    assert!(connect(Some("http://evil.example"), Some(&cookie)).await.is_err(), "bad origin must be refused");
    assert!(connect(None, Some(&cookie)).await.is_err(), "missing origin must be refused");

    let (mut ws, _) = connect(Some(base), Some(&cookie)).await.expect("good upgrade");
    let mut tree = None;
    while let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v["t"] == "tree" {
            tree = Some(v);
            break;
        }
    }
    let tree = tree.expect("a tree message");
    assert_eq!(tree["typing"], false);
    assert_eq!(tree["workspaces"][0]["title"], "shared");
    assert_eq!(tree["workspaces"][0]["tabs"][0]["id"], "surface:1");

    // The live screen arrives once a tab is opened.
    ws.send(Message::Text(r#"{"t":"open","surface":"surface:1"}"#.into())).await.unwrap();
    let mut saw = false;
    while let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v["t"] == "screen" && v["text"].as_str().unwrap_or("").contains("SHARE_42") {
            saw = true;
            break;
        }
    }
    assert!(saw, "screen frame with the output");

    // View-only: typing is refused and nothing reaches the terminal.
    ws.send(Message::Text(r#"{"t":"send","surface":"surface:1","text":"echo TYPED_IN","enter":true}"#.into())).await.unwrap();
    let mut refused = false;
    while let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        if v["t"] == "error" {
            refused = true;
            break;
        }
    }
    assert!(refused, "typing refused");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!d.ok(&["read-screen", "--surface", "surface:1"]).contains("TYPED_IN"));

    let _ = share.kill();
    let _ = share.wait();
}

/// `term.processes` reports each terminal's shell pid, command and agent tag,
/// and `lmux share` turns a tagged tab's transcript into `chat` frames that
/// follow the file.
#[tokio::test(flavor = "multi_thread")]
async fn share_shows_an_agent_transcript_as_chat() {
    use futures_util::{SinkExt, StreamExt};
    use std::io::{BufRead, BufReader, Read, Write};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

    let Some(d) = Daemon::start() else { return };
    d.ok(&["new-workspace", "--name", "agent", "--command", "/bin/sh"]);
    // What the Claude SessionStart hook records.
    d.ok(&["rpc", "agent.session", r#"{"surface":"surface:1","agent":"claude","id":"abc-123"}"#]);

    let procs = d.json(&["rpc", "term.processes"]);
    let t = &procs["terminals"][0];
    assert_eq!(t["surface"], "surface:1", "{procs}");
    assert_eq!(t["agent_session"], "claude:abc-123", "{procs}");
    assert!(t["pid"].as_u64().is_some_and(|p| p > 1), "{procs}");
    assert!(t["command"].as_str().is_some_and(|c| c.ends_with("sh")), "{procs}");

    // A transcript where the share looks for it, under a scratch HOME.
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".claude/projects/-work");
    std::fs::create_dir_all(&dir).unwrap();
    let transcript = dir.join("abc-123.jsonl");
    std::fs::write(
        &transcript,
        concat!(
            r#"{"type":"user","timestamp":"2026-10-11T01:00:00Z","message":{"content":"CHAT_QUESTION"}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"CHAT_ANSWER"}]}}"#, "\n",
        ),
    )
    .unwrap();

    let mut share = d
        .cmd(&["--json", "share", "--no-tunnel"])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(share.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let info: serde_json::Value = serde_json::from_str(&line).expect("share prints JSON");
    let (base, code) = info["url"].as_str().unwrap().split_once("/#").unwrap();
    let (base, code) = (base.to_string(), code.to_string());
    let addr = base.trim_start_matches("http://").to_string();
    let body = format!("{{\"code\":\"{code}\"}}");
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    s.write_all(
        format!("POST /auth HTTP/1.1\r\nHost: {addr}\r\nOrigin: {base}\r\nContent-Length: {}\r\n\r\n{body}", body.len())
            .as_bytes(),
    )
    .unwrap();
    let mut ok = String::new();
    s.read_to_string(&mut ok).unwrap();
    let cookie = ok.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).expect("cookie");
    let cookie = cookie.split(';').next().unwrap().to_string();

    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("Origin", base.parse().unwrap());
    req.headers_mut().insert("Cookie", cookie.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.expect("upgrade");
    let tree = loop {
        let v: serde_json::Value = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str(&t).unwrap(),
            other => panic!("no frame: {other:?}"),
        };
        if v["t"] == "tree" {
            break v;
        }
    };
    let tab = &tree["workspaces"][0]["tabs"][0];
    assert_eq!(tab["agent"], "claude", "{tree}");
    assert_eq!(tab["chat"], true, "{tree}");
    assert!(!tree.to_string().contains("abc-123.jsonl"), "no paths reach the page");

    ws.send(Message::Text(r#"{"t":"open","surface":"surface:1"}"#.into())).await.unwrap();
    let mut chat_with = async |needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["t"] == "chat" && v.to_string().contains(needle) {
                    return v;
                }
            }
        }
        panic!("no chat frame with {needle}");
    };
    let chat = chat_with("CHAT_ANSWER").await;
    assert_eq!(chat["surface"], "surface:1");
    assert_eq!(chat["agent"], "claude");
    assert_eq!(chat["messages"][0]["role"], "user");
    assert_eq!(chat["messages"][0]["text"], "CHAT_QUESTION");
    assert_eq!(chat["messages"][1]["role"], "assistant");

    // A new line in the transcript sends a fresh snapshot.
    let mut f = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
    writeln!(f, r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"CHAT_FOLLOW_UP"}}]}}}}"#).unwrap();
    let chat = chat_with("CHAT_FOLLOW_UP").await;
    assert_eq!(chat["messages"].as_array().unwrap().len(), 3);

    // Replying from the chat view is a `send`, still refused on a view-only share.
    ws.send(Message::Text(r#"{"t":"send","surface":"surface:1","text":"hi","enter":true}"#.into())).await.unwrap();
    let refused = loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) if t.contains(r#""t":"error""#) => break t,
            Ok(Some(Ok(_))) => {}
            other => panic!("no error frame: {other:?}"),
        }
    };
    assert!(refused.contains("Typing is off"), "{refused}");

    let _ = share.kill();
    let _ = share.wait();
}

/// Gateway mode: `lmux share --server X` lists X's sessions next to this
/// machine's, under `X/` ids, and sends screens, input and uploads to the
/// server that owns the tab. A stand-in `ssh` runs the backend command here,
/// against a second scratch daemon that plays the remote server.
#[tokio::test(flavor = "multi_thread")]
async fn share_gateway_controls_another_server() {
    use futures_util::{SinkExt, StreamExt};
    use std::io::{BufRead, BufReader, Read, Write};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

    let Some(local) = Daemon::start() else { return };
    let Some(remote) = Daemon::start() else { return };
    local.ok(&["new-workspace", "--name", "here", "--command", "/bin/sh"]);
    remote.ok(&["new-workspace", "--name", "there", "--command", "/bin/sh"]);

    let bin_dir = tempfile::tempdir().unwrap();
    let ssh = bin_dir.path().join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nfor a; do [ \"$a\" = gone ] && { echo 'ssh: Could not resolve hostname gone' >&2; exit 255; }; last=$a; done\nexec sh -c \"$last\"\n").unwrap();
    std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin_dir.path().display(), std::env::var("PATH").unwrap_or_default());
    let remote_bin = format!("{BIN} --socket {}", remote.socket.display());

    let mut share = local
        .cmd(&["--json", "share", "--no-tunnel", "--allow-typing", "--server", "vps1", "--server", "gone"])
        .env("PATH", &path)
        .env("LMUX_REMOTE_BIN", &remote_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(share.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let info: serde_json::Value = serde_json::from_str(&line).expect("share prints JSON");
    let url = info["url"].as_str().unwrap().to_string();
    let (base, code) = url.split_once("/#").unwrap();
    let addr = base.trim_start_matches("http://").to_string();
    let body = format!("{{\"code\":\"{code}\"}}");
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    s.write_all(
        format!("POST /auth HTTP/1.1\r\nHost: {addr}\r\nOrigin: {base}\r\nContent-Length: {}\r\n\r\n{body}", body.len())
            .as_bytes(),
    )
    .unwrap();
    let mut ok = String::new();
    s.read_to_string(&mut ok).unwrap();
    let cookie = ok.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).expect("cookie").split(';').next().unwrap().to_string();

    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("Origin", base.parse().unwrap());
    req.headers_mut().insert("Cookie", cookie.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.expect("upgrade");

    // Wait for a tree with both servers' sessions (the SSH link comes up in the background).
    let mut tree = serde_json::Value::Null;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        let settled = |n: &str, want: &str| {
            v["servers"].as_array().into_iter().flatten().any(|s| s["name"] == n && s["status"] == want)
        };
        if v["t"] == "tree" && v["workspaces"].as_array().is_some_and(|w| w.len() == 2)
            && settled("vps1", "connected") && settled("gone", "unreachable")
        {
            tree = v;
            break;
        }
    }
    let ws_list = tree["workspaces"].as_array().expect("both servers' workspaces");
    let there = ws_list.iter().find(|w| w["title"] == "there").expect("remote workspace");
    assert_eq!(there["server"], "vps1");
    assert_eq!(there["tabs"][0]["id"], "vps1/surface:1");
    let here = ws_list.iter().find(|w| w["title"] == "here").expect("local workspace");
    assert_eq!(here["tabs"][0]["id"], "surface:1");
    let servers = tree["servers"].as_array().expect("server list");
    let status = |n: &str| servers.iter().find(|s| s["name"] == n).map(|s| s["status"].clone());
    assert_eq!(status("vps1"), Some(serde_json::json!("connected")));
    assert_eq!(status("gone"), Some(serde_json::json!("unreachable")));
    let gone = servers.iter().find(|s| s["name"] == "gone").unwrap();
    assert!(gone["error"].as_str().unwrap_or("").contains("resolve"), "{gone}");

    // Typing into the remote tab reaches the remote daemon only.
    ws.send(Message::Text(r#"{"t":"open","surface":"vps1/surface:1"}"#.into())).await.unwrap();
    ws.send(Message::Text(r#"{"t":"send","surface":"vps1/surface:1","text":"echo GW_$((6*7))","enter":true}"#.into()))
        .await
        .unwrap();
    let mut saw = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !saw {
        let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        saw = v["t"] == "screen" && v["surface"] == "vps1/surface:1" && v["text"].as_str().unwrap_or("").contains("GW_42");
    }
    assert!(saw, "the remote screen shows the typed command's output");
    assert!(!local.ok(&["read-screen", "--surface", "surface:1"]).contains("GW_"));

    // An upload lands on the server of the open tab.
    ws.send(Message::Text(r#"{"t":"upload","id":"u1","name":"note.txt","size":5}"#.into())).await.unwrap();
    ws.send(Message::Binary(b"hello".to_vec())).await.unwrap();
    let mut path = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && path.is_none() {
        let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        assert_ne!(v["t"], "upload_error", "{v}");
        if v["t"] == "uploaded" {
            path = v["path"].as_str().map(str::to_owned);
        }
    }
    let path = path.expect("uploaded");
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");

    // Local IDs must support the same screen/send/key path, without leaking
    // input into the remote daemon when switching hosts on the phone.
    ws.send(Message::Text(r#"{"t":"open","surface":"surface:1"}"#.into())).await.unwrap();
    ws.send(Message::Text(r#"{"t":"send","surface":"surface:1","text":"echo LOCAL_$((6*7))","enter":false}"#.into())).await.unwrap();
    ws.send(Message::Text(r#"{"t":"key","surface":"surface:1","key":"enter"}"#.into())).await.unwrap();
    let mut saw_local = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !saw_local {
        let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        saw_local = v["t"] == "screen" && v["surface"] == "surface:1" && v["text"].as_str().unwrap_or("").contains("LOCAL_42");
    }
    assert!(saw_local, "the local screen shows the typed command's output");
    assert!(!remote.ok(&["read-screen", "--surface", "surface:1"]).contains("LOCAL_"));

    // An unknown server's tab is refused.
    ws.send(Message::Text(r#"{"t":"send","surface":"nope/surface:1","text":"x","enter":false}"#.into())).await.unwrap();

    let _ = share.kill();
    let _ = share.wait();
    let _ = std::fs::remove_file(&path);
}

/// A laptop registers with the gateway using outbound SSH, and a phone can
/// control its real scratch daemon through that connection.
#[test]
fn share_attachment_exits_when_gateway_closes_with_stdin_still_open() {
    use std::io::{BufRead, BufReader, Write};
    let dir = tempfile::Builder::new().prefix("lmuxattach").tempdir_in("/tmp").unwrap();
    let listener = std::os::unix::net::UnixListener::bind(dir.path().join("share.sock")).unwrap();
    let mut child = Command::new(BIN).args(["share-attach", "--name", "macbook"])
        .env("LMUX_STATE_DIR", dir.path()).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).unwrap();
    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(req, serde_json::json!({"op":"attach", "name":"macbook"}));
    stream.write_all(b"{\"attached\":true}\n").unwrap();
    // Make sure the bridge has begun reading stdin before closing the gateway.
    child.stdin.as_mut().unwrap().write_all(b"{\"hello\":1}\n").unwrap();
    line.clear();
    BufReader::new(&mut stream).read_line(&mut line).unwrap();
    assert_eq!(line, "{\"hello\":1}\n");
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() { assert!(status.success()); break; }
        if Instant::now() >= deadline {
            let _ = child.kill(); let _ = child.wait();
            panic!("bridge waited on uncancellable stdin after gateway disconnected");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The SSH connection carries the laptop backend, not an inbound laptop port.
#[tokio::test(flavor = "multi_thread")]
async fn share_gateway_accepts_an_outbound_laptop_backend() {
    use futures_util::{SinkExt, StreamExt};
    use std::io::{Read, Write};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let Some(gateway) = Daemon::start() else { return };
    let Some(laptop) = Daemon::start() else { return };
    gateway.ok(&["new-workspace", "--name", "gateway", "--command", "/bin/sh"]);
    laptop.ok(&["new-workspace", "--name", "laptop", "--command", "/bin/sh"]);
    let dir = tempfile::Builder::new().prefix("lmuxgw").tempdir_in("/tmp").unwrap();
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }
    let _service = OwnedChild(gateway.cmd(&["share", "--service", "--no-tunnel", "--allow-typing"])
        .env("LMUX_STATE_DIR", dir.path()).env("LMUX_VOICE", "0")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.path().join("share.sock").exists() {
        assert!(Instant::now() < deadline, "gateway control socket did not appear");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // SSH stand-in connects to the gateway's real private control socket.
    let ssh = dir.path().join("ssh");
    // Both scratch daemons have this machine's hostname. Give the attachment
    // a distinct name, as separate machines have in production.
    std::fs::write(&ssh, format!("#!/bin/sh\nexec '{BIN}' share-attach --name test-macbook\n")).unwrap();
    std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap());
    let mut connector = OwnedChild(laptop.cmd(&["share-connect", "test-gateway"])
        .env("PATH", path).env("LMUX_REMOTE_BIN", BIN).env("LMUX_STATE_DIR", dir.path())
        .stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap());
    let info = gateway.cmd(&["share-code"]).env("LMUX_STATE_DIR", dir.path()).output().unwrap();
    assert!(info.status.success());
    let info: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
    let (base, code) = info["url"].as_str().unwrap().split_once("/#").unwrap();
    let addr = base.trim_start_matches("http://");
    let body = serde_json::json!({"code":code}).to_string();
    let mut http = std::net::TcpStream::connect(addr).unwrap();
    http.write_all(format!("POST /auth HTTP/1.1\r\nHost: {addr}\r\nOrigin: {base}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
    let mut response = String::new();
    http.read_to_string(&mut response).unwrap();
    let cookie = response.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap().split(';').next().unwrap();
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("Origin", base.parse().unwrap());
    req.headers_mut().insert("Cookie", cookie.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let surface = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let Message::Text(t) = ws.next().await.unwrap().unwrap() else { continue };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if let Some(w) = v["workspaces"].as_array().and_then(|a| a.iter().find(|w| w["title"] == "laptop")) {
                assert_eq!(w["server"], "test-macbook");
                break w["tabs"][0]["id"].as_str().unwrap().to_owned();
            }
        }
    }).await.expect("laptop appears on gateway");
    for message in [serde_json::json!({"t":"open","surface":surface}), serde_json::json!({"t":"send","surface":surface,"text":"echo LAPTOP_$((6*7))","enter":true})] {
        ws.send(Message::Text(message.to_string())).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let Message::Text(t) = ws.next().await.unwrap().unwrap() else { continue };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["t"] == "screen" && v["surface"] == surface && v["text"].as_str().unwrap_or("").contains("LAPTOP_42") { break; }
        }
    }).await.expect("phone input reaches laptop via outbound backend");
    assert!(!gateway.ok(&["read-screen", "--surface", "surface:1"]).contains("LAPTOP_"));
    drop(connector.0.stdin.take());
    assert!(connector.0.wait().unwrap().success(), "app closing stdin stops connector");
}

/// The voice endpoints against a fake OpenAI: auth rules, the session body,
/// and a `read_screen` tool call that runs through the share's real daemon.
#[tokio::test(flavor = "multi_thread")]
async fn share_voice_runs_a_read_only_tool_for_the_call() {
    use futures_util::{SinkExt, StreamExt};
    use std::io::{BufRead, BufReader, Read, Write};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::Message;

    let Some(d) = Daemon::start() else { return };
    d.ok(&["new-workspace", "--name", "shared", "--command", "/bin/sh"]);
    d.ok(&["send", "echo", "VOICE_$((6*7))\\n", "--surface", "surface:1"]);
    d.ok(&["wait-for", "--text", "VOICE_42", "--surface", "surface:1", "--timeout", "10"]);

    // A fake OpenAI, and a voice config that points at it with a throwaway key.
    let fake = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake_base = format!("http://{}", fake.local_addr().unwrap());
    let cfg_dir = tempfile::Builder::new().prefix("lmuxvoice").tempdir_in("/tmp").unwrap();
    let key = cfg_dir.path().join("key");
    std::fs::write(&key, "throwaway-test-key\n").unwrap();
    let cfg = cfg_dir.path().join("voice.json");
    std::fs::write(&cfg, serde_json::json!({"openai_api_key_file": key}).to_string()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [&key, &cfg] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    let server = tokio::spawn(async move {
        let (mut s, _) = fake.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let (end, len) = loop {
            let n = s.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                assert!(head.contains("authorization: bearer throwaway-test-key"));
                let len: usize = head.split("content-length: ").nth(1).unwrap().split("\r\n").next().unwrap().parse().unwrap();
                break (p + 4, len);
            }
        };
        while buf.len() < end + len {
            let n = s.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
        }
        let sent: serde_json::Value = serde_json::from_slice(&buf[end..end + len]).unwrap();
        let body = serde_json::json!({"session": {"id": "live_t"}, "transport": {"sdp": "v=0 answer"}}).to_string();
        s.write_all(format!("HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        drop(s);

        let (s, _) = fake.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
        let ev = |v: serde_json::Value| Message::Text(serde_json::json!({"type": "response.event", "delegation_id": "d", "event": v}).to_string());
        ws.send(ev(serde_json::json!({"type": "response.created", "response": {"id": "r1"}}))).await.unwrap();
        ws.send(ev(serde_json::json!({"type": "response.output_item.done", "item": {"type": "function_call",
            "call_id": "c1", "name": "read_screen", "arguments": "{\"surface\":\"surface:1\"}"}}))).await.unwrap();
        ws.send(ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1", "output": []}}))).await.unwrap();
        let mut output = None;
        let mut closed_asked = false;
        while let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            match v["type"].as_str() {
                Some("response.item.create") => output = Some(v["item"]["output"].as_str().unwrap().to_string()),
                Some("session.close") => {
                    closed_asked = true;
                    ws.send(Message::Text(serde_json::json!({"type": "session.closed", "reason": "close_requested", "usage": {"seconds": 1}}).to_string())).await.unwrap();
                    break;
                }
                _ => {}
            }
        }
        (sent, output, closed_asked)
    });

    let mut share = d
        .cmd(&["--json", "share", "--no-tunnel"])
        .env("LMUX_VOICE_CONFIG", &cfg)
        .env("LMUX_OPENAI_BASE", &fake_base)
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(share.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let info: serde_json::Value = serde_json::from_str(&line).unwrap();
    let url = info["url"].as_str().unwrap().to_string();
    let (base, code) = url.split_once("/#").unwrap();
    let addr = base.trim_start_matches("http://").to_string();
    let http = |req: String| -> String {
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    };
    let post = |path: &str, origin: &str, cookie: Option<&str>, body: &str| {
        let cookie = cookie.map(|c| format!("Cookie: {c}\r\n")).unwrap_or_default();
        http(format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nOrigin: {origin}\r\n{cookie}Content-Length: {}\r\n\r\n{body}", body.len()))
    };

    // Nothing without the cookie or with a foreign origin, and nothing was spent upstream.
    let offer = r#"{"sdp":"v=0\r\no=- 1 1 IN IP4 0.0.0.0"}"#;
    assert!(post("/voice/offer", base, None, offer).starts_with("HTTP/1.1 401"));
    let ok = post("/auth", base, None, &format!("{{\"code\":\"{code}\"}}"));
    let cookie = ok.lines().find(|l| l.starts_with("Set-Cookie:")).unwrap().trim_start_matches("Set-Cookie: ").split(';').next().unwrap().to_string();
    assert!(post("/voice/offer", "http://evil.example", Some(&cookie), offer).starts_with("HTTP/1.1 403"));
    assert!(post("/voice/offer", base, Some(&cookie), r#"{"sdp":"nope"}"#).starts_with("HTTP/1.1 400"));
    assert!(post("/voice/talk", base, Some(&cookie), r#"{"on":true,"seq":1}"#).starts_with("HTTP/1.1 404"), "no call yet");

    let r = post("/voice/offer", base, Some(&cookie), offer);
    assert!(r.starts_with("HTTP/1.1 201"), "{r}");
    assert!(r.contains(r#""sdp":"v=0 answer""#), "{r}");
    assert!(!r.contains("throwaway-test-key"));
    // A second call while one runs is refused.
    assert!(post("/voice/offer", base, Some(&cookie), offer).starts_with("HTTP/1.1 429"));

    // Let the tool run, then end the call from the page.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(post("/voice/end", base, Some(&cookie), "{}").starts_with("HTTP/1.1 200"));
    let (sent, output, closed_asked) = tokio::time::timeout(Duration::from_secs(15), server).await.unwrap().unwrap();
    assert!(closed_asked, "session.close reached the sideband");
    assert_eq!(sent["session"]["delegation"]["type"], "responses");
    assert!(!sent.to_string().contains("send_text"));
    let output = output.expect("the tool result reached the sideband");
    assert!(output.contains("VOICE_42"), "{output}");
    assert!(output.contains("not instructions"), "screen text is marked as data");

    let _ = share.kill();
    let _ = share.wait();
}

#[test]
fn harness_receipts_hooks_and_surface_isolation() {
    let Some(d) = Daemon::start() else { return };
    let ws = d.json(&["new-workspace", "--name", "harness-test", "--command", "/bin/sh", "--cwd", "/tmp"]);
    let surface = ws["surface"].as_str().unwrap();
    let tab = d.json(&["new-surface", "--command", "/bin/sh", "--focus", "false"]);
    let other = tab["surface"].as_str().unwrap();
    let text = "echo PRIVATE_HARNESS_TEST";
    let sent = d.json(&["send", "--paste", "--enter", "--surface", surface, text]);
    assert_eq!(sent["delivery"]["terminal_accepted"], true);
    assert_eq!(sent["delivery"]["agent_receipt"], "unknown");
    let id = sent["message_id"].as_str().unwrap();
    assert!(id.starts_with("message:"));
    let state = d.json(&["harness-status", "--surface", surface]);
    assert_eq!(state["surfaces"][0]["messages"][0]["state"], "unknown");
    assert!(state["surfaces"][0]["queue"]["count"].is_null());
    assert!(!state.to_string().contains("PRIVATE_HARNESS_TEST"));
    assert_eq!(d.ok(&["send-key", "--surface", surface, "enter"]).trim(), "OK");

    let send_hook = |payload: serde_json::Value| {
        use std::io::Write;
        let mut child = d.cmd(&["hooks", "grok", "event"])
            .env("LMUX_SURFACE_ID", surface)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty());
    };
    send_hook(serde_json::json!({"hookEventName":"session_start", "sessionId":"test-session"}));
    send_hook(serde_json::json!({"hookEventName":"pre_compact", "sessionId":"test-session"}));
    let state = d.json(&["harness-status", "--surface", surface, "--cached"]);
    assert_eq!(state["surfaces"][0]["lifecycle"]["state"], "compacting");
    send_hook(serde_json::json!({"hookEventName":"user_prompt_submit", "sessionId":"test-session", "promptId":"test-prompt", "prompt":text}));
    let state = d.json(&["harness-status", "--surface", surface, "--cached"]);
    assert_eq!(state["surfaces"][0]["messages"][0]["id"], id);
    assert_eq!(state["surfaces"][0]["messages"][0]["state"], "submitted");
    assert!(state["surfaces"][0]["messages"][0]["consumed_at_ms"].is_null());
    send_hook(serde_json::json!({"hookEventName":"notification", "sessionId":"test-session", "notificationType":"permission_prompt"}));
    send_hook(serde_json::json!({"hookEventName":"stop", "sessionId":"child-session", "subagentType":"explore"}));
    let state = d.json(&["harness-status", "--surface", surface, "--cached"]);
    assert_eq!(state["surfaces"][0]["lifecycle"]["state"], "waiting");
    let state = d.json(&["harness-status", "--surface", other, "--cached"]);
    assert!(state["surfaces"][0]["messages"].as_array().unwrap().is_empty());
    let state = d.json(&["harness-status", "--workspace", ws["workspace"].as_str().unwrap(), "--cached"]);
    assert_eq!(state["surfaces"].as_array().unwrap().len(), 2);
    let bad = d.run(&["harness-status", "--surface", "999999"]);
    assert!(!bad.status.success());
    let config = d.json(&["hooks", "grok", "print-config"]);
    assert!(config["hooks"]["PostCompact"].is_array());
    d.ok(&["close-surface", "--surface", surface]);
    send_hook(serde_json::json!({"hookEventName":"session_start", "sessionId":"late-closed-session"}));
    let other_state = d.json(&["harness-status", "--surface", other, "--cached"]);
    assert!(other_state["surfaces"][0]["session_id"].is_null());
    assert_eq!(other_state["surfaces"][0]["lifecycle"]["state"], "unknown");
}

#[test]
fn harness_configuration_query_and_safe_unsupported_set() {
    let Some(d) = Daemon::start() else { return };
    let ws = d.json(&["new-workspace", "--name", "configuration-test", "--command", "/bin/sh", "--cwd", "/tmp"]);
    let surface = ws["surface"].as_str().unwrap();
    let empty = d.json(&["harness-config", "--surface", surface, "--options"]);
    assert!(empty["configuration"]["applied"]["model"].is_null());
    assert!(empty["configuration"]["options"]["models"].is_null());
    use std::io::Write;
    let mut child = d.cmd(&["hooks", "grok", "status-line"])
        .env("LMUX_SURFACE_ID", surface)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"session_id":"configuration-session","prompt_id":"active-prompt","model":{"id":"old-model"},"effort":{"level":"low"},"trigger":"state"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    let observed = d.json(&["harness-config", "--surface", surface]);
    assert_eq!(observed["configuration"]["applied"]["model"], "old-model");
    assert_eq!(observed["configuration"]["applied"]["evidence"]["source"], "grok_status_line");
    let _ = d.json(&["rpc", "harness.observe", &serde_json::json!({"surface":surface,"event":"pre_compact","session_id":"configuration-session"}).to_string()]);
    let before = d.ok(&["read-screen", "--surface", surface]);
    let out = d.run(&["--json", "harness-config", "--surface", surface, "--model", "gpt-6.1-sol", "--effort", "high"]);
    assert!(!out.status.success());
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(response["configuration"]["requested"]["state"], "unsupported");
    assert_eq!(response["configuration"]["requested"]["model"], "gpt-6.1-sol");
    assert_eq!(response["configuration"]["requested"]["effort"], "high");
    assert_eq!(response["configuration"]["requested"]["sent_to_harness"], false);
    assert_eq!(response["configuration"]["applied"]["model"], "old-model");
    assert_eq!(before, d.ok(&["read-screen", "--surface", surface]));
    let state = d.json(&["harness-status", "--surface", surface, "--cached"]);
    assert_eq!(state["surfaces"][0]["lifecycle"]["state"], "compacting");
    assert!(state["surfaces"][0]["messages"].as_array().unwrap().is_empty());
    let out = d.run(&["--json", "harness-config", "--surface", surface, "--effort", "high;bad"]);
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(response["configuration"]["requested"]["state"], "failed");
    assert!(response["configuration"]["requested"]["effort"].is_null());
}

#[test]
fn harness_visible_queue_refresh_uses_foreground_grok_and_structured_json() {
    let Some(d) = Daemon::start() else { return };
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("grok-fixture");
    std::fs::copy("/bin/sleep", &fixture).unwrap();
    #[cfg(target_os = "macos")]
    assert!(Command::new("codesign").args(["--force", "--sign", "-"]).arg(&fixture)
        .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success());
    let command = format!("printf '1. steer alpha\\n2. steer bravo\\n3. steer charlie\\n3 queued, Enter to send now\\nCompacting...\\n'; exec '{}' 60", fixture.display());
    let ws = d.json(&["new-workspace", "--name", "queue-fixture", "--command", &command, "--cwd", "/tmp"]);
    let surface = ws["surface"].as_str().unwrap();
    d.ok(&["wait-for", "--text", "Enter to send now", "--surface", surface, "--timeout", "10"]);
    let a = d.json(&["send", "--surface", surface, "--paste", "--enter", "steer alpha"]);
    let b = d.json(&["send", "--surface", surface, "--paste", "--enter", "steer bravo"]);
    let c = d.json(&["send", "--surface", surface, "--paste", "--enter", "steer charlie"]);
    let v = d.json(&["harness-status", "--surface", surface]);
    assert_eq!(v["surfaces"][0]["harness"], "grok", "{v}");
    assert_eq!(v["surfaces"][0]["lifecycle"]["state"], "compacting");
    assert_eq!(v["surfaces"][0]["queue"]["count"], 3);
    assert_eq!(v["surfaces"][0]["queue"]["evidence"]["confidence"], "heuristic");
    for (index, sent) in [a, b, c].into_iter().enumerate() {
        assert_eq!(v["surfaces"][0]["messages"][index]["id"], sent["message_id"]);
        assert_eq!(v["surfaces"][0]["messages"][index]["state"], "queued");
        assert!(v["surfaces"][0]["messages"][index]["consumed_at_ms"].is_null());
    }
}

/// A private SSH stand-in executes the actual quoted remote command against a
/// second scratch daemon, with deliberately conflicting inherited context.
struct HostCliFixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    remote_home: PathBuf,
    bin: PathBuf,
}

impl HostCliFixture {
    fn new(local: &Daemon, remote: &Daemon, registry: serde_json::Value) -> Self {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::Builder::new().prefix("lmux-host-cli-").tempdir_in("/tmp").unwrap();
        let home = dir.path().join("home");
        let remote_home = dir.path().join("remote");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for (home, daemon) in [(&home, local), (&remote_home, remote)] {
            std::fs::create_dir_all(home.join(".local/bin")).unwrap();
            std::fs::create_dir_all(home.join(".local/state/lmux")).unwrap();
            symlink(BIN, home.join(".local/bin/lmux")).unwrap();
            symlink(&daemon.socket, home.join(".local/state/lmux/lmux.sock")).unwrap();
        }
        let config = if cfg!(target_os = "macos") { home.join("Library/Application Support/lmux") } else { home.join(".config/lmux") };
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("hosts.json"), registry.to_string()).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/config"), "Host box\nHost *.invalid !skip\n").unwrap();
        let ssh = bin.join("ssh");
        std::fs::write(&ssh, r##"#!/bin/sh
previous=''
last=''
for arg do previous=$last; last=$arg; done
if [ -n "$TEST_SSH_LOG" ]; then printf '%s\n' "$previous" >> "$TEST_SSH_LOG"; fi
case "$previous" in
  slow) exec /bin/sleep 2;;
  dead) echo 'test: peer unreachable' >&2; exit 255;;
  invalid) printf 'invalid-json\n'; exit 0;;
esac
export HOME="$TEST_REMOTE_HOME"
unset XDG_CONFIG_HOME XDG_STATE_HOME
export LMUX_SOCKET=/wrong-socket LMUX_STATE_DIR=/wrong-state LMUX_SURFACE_ID=surface:1 LMUX_WORKSPACE_ID=workspace:1
exec /bin/sh -c "$last"
"##).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { _dir: dir, home, remote_home, bin }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command.args(args).env("HOME", &self.home).env("TEST_REMOTE_HOME", &self.remote_home)
            .env("PATH", format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap_or_default()))
            .env_remove("XDG_CONFIG_HOME").env_remove("XDG_STATE_HOME")
            .env_remove("LMUX_SOCKET").env_remove("LMUX_STATE_DIR")
            .env_remove("LMUX_WORKSPACE_ID").env_remove("LMUX_SURFACE_ID");
        command
    }
}

#[test]
fn host_cli_routes_colliding_refs_and_stdin_without_local_caller_context() {
    use std::io::Write;
    let (Some(local), Some(remote)) = (Daemon::start(), Daemon::start()) else { return };
    let local_ws = local.json(&["new-workspace", "--name", "local-work", "--command", "/bin/cat", "--cwd", "/tmp"]);
    let remote_ws = remote.json(&["new-workspace", "--name", "remote-work", "--command", "/bin/cat", "--cwd", "/tmp"]);
    assert_eq!(local_ws["surface"], remote_ws["surface"]);
    let focused = remote.json(&["new-surface", "--command", "/bin/cat", "--focus", "true"]);
    let fixture = HostCliFixture::new(&local, &remote, serde_json::json!([
        {"name":"build", "label":"Build Host", "kind":"ssh", "ssh_target":"box"}
    ]));
    let out = fixture.cmd(&["--host", "Build Host", "--json", "identify"])
        .env("LMUX_SOCKET", &local.socket).env("LMUX_STATE_DIR", "/wrong-local-state")
        .env("LMUX_SURFACE_ID", local_ws["surface"].as_str().unwrap())
        .env("LMUX_WORKSPACE_ID", local_ws["workspace"].as_str().unwrap()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let identified: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(identified["surface"], focused["surface"]);
    assert_eq!(identified["cli_target"]["host"], "Build Host");
    let out = fixture.cmd(&["--json", "identify"]).env("LMUX_SURFACE_ID", local_ws["surface"].as_str().unwrap()).output().unwrap();
    assert!(out.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["surface"], local_ws["surface"]);

    let surface = remote_ws["surface"].as_str().unwrap();
    let marker = fixture.home.join("shell-injection");
    let text = format!("literal --host 'quote' `id` ~ $(touch '{}')\nsecond line", marker.display());
    let mut child = fixture.cmd(&["--host=build", "--json", "paste", "--surface", surface, "-"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!marker.exists());
    let out = fixture.cmd(&["--host", "build", "wait-for", "--surface", surface, "--text", "literal --host", "--timeout", "5"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = fixture.cmd(&["--host", "build", "--socket", remote.socket.to_str().unwrap(), "--json", "tree"]).output().unwrap();
    assert!(out.status.success());
    let tree: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tree["workspaces"][0]["title"], "remote-work");
    assert_eq!(local.json(&["tree"])["workspaces"][0]["title"], "local-work");
    let out = fixture.cmd(&["--host", "build", "read-screen", "--surface", "999999"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("host build"));
}

#[test]
fn host_cli_discovery_and_aggregate_trees_preserve_partial_failures_and_host_scope() {
    let (Some(local), Some(remote)) = (Daemon::start(), Daemon::start()) else { return };
    local.json(&["new-workspace", "--name", "local-work", "--command", "/bin/cat", "--cwd", "/tmp"]);
    remote.json(&["new-workspace", "--name", "remote-work", "--command", "/bin/cat", "--cwd", "/tmp"]);
    let fixture = HostCliFixture::new(&local, &remote, serde_json::json!([
        {"name":"build", "kind":"ssh", "ssh_target":"box"},
        {"name":"dead", "kind":"ssh", "ssh_target":"dead"},
        {"name":"invalid", "kind":"ssh", "ssh_target":"invalid"},
        {"name":"app-only", "kind":"direct", "url":"ws://localhost:1"}
    ]));
    let out = fixture.cmd(&["--json", "list-hosts"]).output().unwrap();
    assert!(out.status.success());
    let found: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(found["hosts"].as_array().unwrap().len(), 5);
    let out = fixture.cmd(&["--json", "tree", "--all-hosts"]).output().unwrap();
    assert!(!out.status.success());
    let aggregate: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(aggregate["partial"], true);
    let hosts = aggregate["hosts"].as_array().unwrap();
    assert_eq!(hosts[0]["name"], "local");
    assert_eq!(hosts[0]["tree"]["workspaces"][0]["title"], "local-work");
    assert_eq!(hosts[1]["tree"]["workspaces"][0]["title"], "remote-work");
    assert_eq!(hosts[0]["tree"]["workspaces"][0]["id"], hosts[1]["tree"]["workspaces"][0]["id"]);
    assert_eq!(hosts[2]["state"], "unreachable");
    assert_eq!(hosts[3]["state"], "unreachable");
    assert_eq!(hosts[4]["state"], "unsupported");
    let out = fixture.cmd(&["--host", "dead", "--json", "tree"]).output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    for args in [vec!["--host=-bad", "tree"], vec!["--host=build", "server"], vec!["--host=build", "hooks", "grok", "print-config"], vec!["--host=build", "wait-for", "-S", "local-signal"]] {
        assert!(!fixture.cmd(&args).output().unwrap().status.success());
    }
}

#[test]
fn host_cli_bounds_remote_waits_and_large_requests_and_keeps_explicit_discovery_working() {
    use std::io::Write;
    let (Some(local), Some(remote)) = (Daemon::start(), Daemon::start()) else { return };
    let fixture = HostCliFixture::new(&local, &remote, serde_json::json!([]));
    let log = fixture.home.join("ssh-calls");
    let mut child = fixture.cmd(&["--host", "box", "paste", "-"])
        .env("TEST_SSH_LOG", &log).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&vec![b'x'; 256 * 1024]).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("64 KiB"));
    assert!(!log.exists(), "oversized request launched SSH");
    let started = Instant::now();
    let out = fixture.cmd(&["--host", "slow", "wait-for", "--text", "late", "--timeout", "0.05"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(1));
    let aliases = (0..300).map(|i| format!("Host box-{i}\n")).collect::<String>();
    std::fs::write(fixture.home.join(".ssh/config"), aliases).unwrap();
    let out = fixture.cmd(&["--host", "box-299", "ping"]).env("TEST_SSH_LOG", &log).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    for args in [vec!["--host", "box-299", "tree", "--all-hosts"], vec!["tree", "--all-hosts", "--host", "box-299"], vec!["--socket", "/x", "tree", "--all-hosts"], vec!["tree", "--all-hosts"]] {
        let out = fixture.cmd(&args).env("TEST_SSH_LOG", &log).output().unwrap();
        assert!(!out.status.success());
    }
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
}
