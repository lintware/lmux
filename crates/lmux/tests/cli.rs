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
