//! End-to-end copy and paste against a real host, through the app's own
//! networking, state and terminal code (no window).
//!
//!   cargo run --example remote_clipboard -- [host]     (default: my-server)
//!
//! Creates a throwaway workspace on the host, then checks:
//!   1. paste fidelity: a large, multi-line, unicode/quote/tab-heavy text pasted
//!      into `cat > file` arrives byte for byte (read back over ssh);
//!   2. bracketed paste: a multi-line command pasted at a bash prompt does not
//!      run until Enter;
//!   3. OSC 52: text a remote program copies (`printf '\e]52;c;…'`) reaches
//!      the local clipboard path;
//!   4. selection copy: text on the remote screen can be selected and copied.
//! The workspace is closed at the end.

use std::time::{Duration, Instant};

use ewebsock::WsMessage;
use lmux_app::net::{ConnState, Net};
use lmux_app::state::AppState;
use lmux_app::wire;

struct Rig {
    net: Net,
    state: AppState,
    ctx: egui::Context,
}

impl Rig {
    /// Pump the connection for `d`, applying everything that arrives.
    fn pump_for(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pump(&mut self) {
        for msg in self.net.poll(&self.ctx) {
            match msg {
                WsMessage::Text(t) => {
                    if let Some(sm) = wire::decode_server(&t) {
                        self.state.handle_server_msg(sm);
                    }
                }
                WsMessage::Binary(b) => self.state.handle_frame(&b),
                _ => {}
            }
        }
        self.state.poll_decoded();
        for m in std::mem::take(&mut self.state.pending) {
            self.net.send(&m);
        }
    }

    fn wait(&mut self, what: &str, timeout: Duration, mut done: impl FnMut(&mut Self) -> bool) {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            self.pump();
            if done(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        panic!("timed out waiting for {what}");
    }

    /// The visible screen of a surface, as text.
    fn screen(&self, sid: &str) -> String {
        let Some(tv) = self.state.terms.get(sid) else { return String::new() };
        (0..tv.rows() as usize).map(|r| tv.row_chars(r).into_iter().collect::<String>().trim_end().to_owned()).collect::<Vec<_>>().join("\n")
    }

    fn type_text(&mut self, sid: &str, text: &str) {
        self.state.send_term_input(sid, text.as_bytes());
        self.pump();
    }

    fn paste(&mut self, sid: &str, text: &str) {
        let bracketed = self
            .state
            .terms
            .get(sid)
            .is_some_and(|t| t.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE));
        let bytes = lmux_app::termio::paste_bytes(text, bracketed);
        self.state.send_term_input(sid, &bytes);
        self.pump();
    }
}

/// Run `cmd` on the host: over ssh, or here for this Mac.
fn on_host(host: Option<&str>, cmd: &str) -> String {
    let out = match host {
        Some(h) => std::process::Command::new("ssh").args(["-o", "BatchMode=yes", h, cmd]).output(),
        None => std::process::Command::new("sh").args(["-c", cmd]).output(),
    }
    .expect("run");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn main() {
    let want = std::env::args().nth(1).unwrap_or_else(|| "my-server".into());
    let hosts = lmux_app::net::app_hosts();
    let index = hosts.iter().position(|h| h.name == want).unwrap_or_else(|| panic!("no host {want} in hosts.json"));
    let local = hosts[index].name == lmux_app::net::LOCAL_HOST_NAME;
    let ssh_target = (!local).then(|| hosts[index].ssh_target.clone().unwrap_or_else(|| want.clone()));
    let ctx = egui::Context::default();
    let mut rig = Rig { net: Net::for_host(hosts, index), state: AppState::new(), ctx: ctx.clone() };
    rig.net.begin_connect(&ctx);
    rig.wait("the connection", Duration::from_secs(30), |r| r.net.state == ConnState::Connected);
    rig.state.set_current_host(&rig.net.host_name());
    rig.wait("the first tree", Duration::from_secs(10), |r| !r.state.tree.host.is_empty());
    println!("connected to {want}");

    // A throwaway workspace with a plain bash, so the test owns its shell.
    let before: Vec<String> = rig.state.tree.workspaces.iter().map(|w| w.id.clone()).collect();
    rig.state.rpc(
        "workspace.new",
        // macOS's /bin/bash 3.2 predates bracketed paste; zsh has it.
        serde_json::json!({
            "name": "lmux-clipboard-test",
            "command": if local { "env PS1='READY$ ' zsh -f" } else { "env PS1='READY$ ' bash --norc --noprofile" },
        }),
    );
    // Ours by name as well as new, so a workspace someone opens in the app
    // meanwhile is never touched.
    let ours = |w: &lmux_proto::Workspace| !before.contains(&w.id) && w.title == "lmux-clipboard-test";
    rig.wait("the new workspace", Duration::from_secs(10), |r| r.state.tree.workspaces.iter().any(ours));
    let ws = rig.state.tree.workspaces.iter().find(|w| ours(w)).unwrap().clone();
    let sid = ws.panes[0].surfaces[0].id.clone();
    println!("workspace {} surface {sid}", ws.id);

    // Show it: subscribe as the app does for a visible pane.
    rig.state.desired_sizes.insert(sid.clone(), (100, 30));
    rig.state.visible.insert(sid.clone());
    rig.state.ensure_term(&sid);
    rig.state.sync_subscriptions(true);
    rig.pump();
    rig.wait("the prompt", Duration::from_secs(10), |r| r.screen(&sid).contains("READY$"));

    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "PASS" } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok {
            failures += 1;
        }
    };

    // 1. Paste fidelity into cat.
    let mut payload = String::new();
    for i in 0..300 {
        payload.push_str(&format!(
            "{i:04} \"quotes\" 'single' $HOME `tick` \\back\\slash\ttab  ✓ ünïcödé 漢字 🚀 {}\n",
            "x".repeat(i % 37)
        ));
    }
    rig.type_text(&sid, "stty -echo; cat > /tmp/lmux-paste-test.txt\r");
    rig.pump_for(Duration::from_millis(500));
    rig.paste(&sid, &payload);
    rig.pump_for(Duration::from_millis(1500));
    rig.type_text(&sid, "\u{4}");
    rig.pump_for(Duration::from_millis(700));
    rig.type_text(&sid, "stty echo\r");
    let got = on_host(ssh_target.as_deref(), "cat /tmp/lmux-paste-test.txt; rm -f /tmp/lmux-paste-test.txt");
    let first_diff = payload.lines().zip(got.lines()).position(|(a, b)| a != b);
    check(
        &format!("paste of {} bytes / {} lines arrives byte for byte", payload.len(), payload.lines().count()),
        got == payload,
        format!("got {} bytes, {} lines; first differing line {first_diff:?}", got.len(), got.lines().count()),
    );

    // 2. Bracketed paste at a bash prompt: nothing runs before Enter.
    rig.pump_for(Duration::from_millis(500));
    rig.wait("bracketed paste mode at the prompt", Duration::from_secs(5), |r| {
        r.state.terms.get(&sid).is_some_and(|t| t.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE))
    });
    rig.paste(&sid, "echo PASTE_ONE\necho PASTE_TWO");
    rig.pump_for(Duration::from_millis(1200));
    let screen = rig.screen(&sid);
    let ran_early = screen.lines().any(|l| l.trim() == "PASTE_ONE" || l.trim() == "PASTE_TWO");
    check("a multi-line paste at a prompt waits for Enter", !ran_early, screen.clone());
    rig.type_text(&sid, "\r");
    rig.pump_for(Duration::from_millis(1200));
    let screen = rig.screen(&sid);
    check(
        "after Enter both pasted lines run",
        screen.lines().any(|l| l.trim() == "PASTE_ONE") && screen.lines().any(|l| l.trim() == "PASTE_TWO"),
        screen.clone(),
    );

    // 3. OSC 52 from the remote program to the local clipboard.
    rig.type_text(&sid, "clear\r");
    rig.pump_for(Duration::from_millis(500));
    let copied = "copied on the server ✓ 漢字";
    rig.type_text(&sid, "printf '\\033]52;c;%s\\a' \"$(printf 'copied on the server ✓ 漢字' | base64 | tr -d '\\n')\"; echo OSC_SENT\r");
    rig.pump_for(Duration::from_millis(1500));
    let clip = rig.state.take_clipboard_requests();
    check(
        "OSC 52 copy on the server reaches the laptop clipboard",
        clip.iter().any(|c| c == copied),
        format!("clipboard requests: {clip:?}"),
    );

    // 4. Select text on the remote screen and copy it.
    rig.type_text(&sid, "clear; echo SELECT_ME_PLEASE\r");
    rig.pump_for(Duration::from_millis(1000));
    let row = rig.screen(&sid).lines().position(|l| l == "SELECT_ME_PLEASE");
    let selected = row.and_then(|r| {
        let tv = rig.state.terms.get_mut(&sid)?;
        tv.start_selection(alacritty_terminal::selection::SelectionType::Simple, r, 0, false);
        tv.update_selection(r, "SELECT_ME_PLEASE".len() - 1, true);
        tv.selection_text()
    });
    check(
        "a selection of remote output copies its text",
        selected.as_deref().map(str::trim) == Some("SELECT_ME_PLEASE"),
        format!("row {row:?}, selection {selected:?}"),
    );

    rig.state.rpc("workspace.close", serde_json::json!({ "workspace": ws.id }));
    rig.pump_for(Duration::from_millis(500));
    println!("{}", if failures == 0 { "ALL PASSED" } else { "SOME FAILED" });
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
