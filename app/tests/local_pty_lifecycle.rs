//! End-to-end lifecycle of a local pane, against a scratch tmux server.
//!
//! This drives the same state the app does: mark the host local, report the pane
//! visible with a size, and let `sync_subscriptions` attach. It then checks the
//! three claims the app relies on — output arrives into the surface's terminal
//! view, the PTY's size drives the tmux window, and the view session is gone as
//! soon as the surface leaves the tree.
//!
//! The scratch server lives on its own socket; the app's own lmux socket is
//! never touched.

use std::time::{Duration, Instant};

use lmux_app::pty::{kill_scratch_server, tmux_bin};

/// Kills the scratch server however the test ends.
struct Scratch<'a>(&'a str, &'a str);

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        kill_scratch_server(self.0, self.1);
    }
}
use lmux_app::state::AppState;
use lmux_app::wire::{AgentState, Layout, Pane, Surface, SurfaceKind, Tree, Workspace};

const SURFACE: &str = "surface:1";

fn run(tmux: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(tmux)
        .args(args)
        .output()
        .map_err(|e| format!("{tmux}: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn out(tmux: &str, args: &[&str]) -> String {
    run(tmux, args).unwrap_or_default().trim().to_owned()
}

/// One workspace, one pane, one terminal surface backed by `pane`.
fn tree(pane: &str) -> Tree {
    let surface = Surface {
        id: SURFACE.into(),
        title: "term".into(),
        kind: SurfaceKind::Terminal { tmux_pane: Some(pane.into()), cols: 80, rows: 24 },
    };
    let p = Pane {
        id: "pane:1".into(),
        surfaces: vec![surface],
        active_surface: Some(SURFACE.into()),
    };
    let ws = Workspace {
        id: "workspace:1".into(),
        title: "w".into(),
        folder: None,
        cwd: "/tmp".into(),
        git_branch: None,
        agent: AgentState::None,
        summary: None,
        unread_notifications: 0,
        status_pills: vec![],
        progress: None,
        layout: Layout::Pane { pane: "pane:1".into() },
        panes: vec![p],
        focused_pane: Some("pane:1".into()),
        ports: vec![],
        pr: None,
    };
    Tree {
        host: "this mac".into(),
        folders: vec![],
        workspaces: vec![ws],
        selected_workspace: Some("workspace:1".into()),
    }
}

/// The visible grid as text, the same way the painter reads it.
fn screen(state: &AppState) -> String {
    let Some(tv) = state.terms.get(SURFACE) else { return String::new() };
    tv.row_chars(0).into_iter().collect()
}

/// Pump the way the app's frame loop does until the pane's screen shows `needle`.
fn wait_screen(state: &mut AppState, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        state.pump_local_ptys();
        if screen(state).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn a_local_pane_attaches_resizes_and_detaches_with_its_surface() {
    let tmux = tmux_bin();
    if std::process::Command::new(&tmux).arg("-V").output().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("skipping: tmux is not available");
        return;
    }
    let socket = format!("lmux-lifecycle-{}", std::process::id());
    // The app's attach reads this; the real lmux socket is never used.
    std::env::set_var("LMUX_TMUX_SOCKET", &socket);
    let _ = run(&tmux, &["-L", &socket, "kill-server"]);
    let _scratch = Scratch(&tmux, &socket);
    // A pane that is in raw mode, so the only echo is the process's.
    run(
        &tmux,
        &["-L", &socket, "new-session", "-d", "-s", "lmux", "-x", "80", "-y", "24", "sh", "-c", "stty raw; printf READY; exec cat"],
    )
    .expect("scratch tmux server");
    let pane = out(&tmux, &["-L", &socket, "display-message", "-p", "-t", "lmux", "#{pane_id}"]);
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");

    let mut state = AppState::new();
    state.set_local_mode(true);
    state.apply_tree(tree(&pane));
    // What the painter reports for a pane that is on screen at 100 columns.
    state.ensure_term(SURFACE);
    state.visible.insert(SURFACE.into());
    state.desired_sizes.insert(SURFACE.into(), (100, 30));

    state.sync_subscriptions(true);
    assert!(
        state.local_ptys.contains_key(SURFACE),
        "a visible local pane attaches a PTY"
    );
    assert!(
        state.pending.is_empty(),
        "nothing about a local pane goes to the daemon: {:?}",
        state.pending
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && !out(&tmux, &["-L", &socket, "list-sessions", "-F", "#{session_name}"])
            .lines()
            .any(|s| s == "view-surface-1")
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    let sessions = out(&tmux, &["-L", &socket, "list-sessions", "-F", "#{session_name}"]);
    assert!(
        sessions.lines().any(|s| s == "view-surface-1"),
        "the pane should be attached through its own grouped session, got {sessions:?} \
         (alive: {:?})",
        state.local_ptys.get(SURFACE).map(|p| p.tmux_pane.clone())
    );

    // Output from the pane reaches the surface's terminal view.
    assert!(
        wait_screen(&mut state, "READY", Duration::from_secs(5)),
        "the pane's output never reached the view: {:?}",
        screen(&state)
    );

    // Writes go straight to the PTY and come back.
    state.write_local(SURFACE, b"Z");
    assert!(
        wait_screen(&mut state, "READYZ", Duration::from_secs(5)),
        "the echo never reached the view: {:?}",
        screen(&state)
    );

    // The PTY's size drives the tmux window (window-size latest), not the other
    // way around: no TermResize was ever sent.
    let width = |tmux: &str| out(tmux, &["-L", &socket, "display-message", "-p", "-t", &pane, "#{pane_width}"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && width(&tmux) != "100" {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(width(&tmux), "100", "the attach should size the window to the pane");

    state.desired_sizes.insert(SURFACE.into(), (60, 20));
    state.sync_subscriptions(true);
    assert!(state.pending.is_empty(), "a local resize is not a daemon resize");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && width(&tmux) != "60" {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(width(&tmux), "60", "the PTY resize should reach the tmux window");

    // The surface leaves the tree: the attachment goes with it, and so does the
    // view session (destroy-unattached).
    state.apply_tree(Tree::default());
    assert!(state.local_ptys.is_empty(), "the PTY is dropped with its surface");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let sessions = out(&tmux, &["-L", &socket, "list-sessions", "-F", "#{session_name}"]);
        if !sessions.lines().any(|s| s == "view-surface-1") {
            break;
        }
        assert!(Instant::now() < deadline, "the view session outlived the pane: {sessions:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
