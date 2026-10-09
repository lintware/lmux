//! `egui_kittest` image snapshots of the hand-painted UI.
//!
//! Run `UPDATE_SNAPSHOTS=1 cargo test --test snapshots` to (re)create the
//! reference images under `tests/snapshots/`.

use egui::{Modifiers, Pos2, Vec2};
use egui_kittest::Harness;
use lmux_app::net::{ConnState, HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::term::TermView;
use lmux_app::{app, fonts, wire};

fn term(id: &str, title: &str, cols: u16, rows: u16) -> wire::Surface {
    wire::Surface {
        id: id.into(),
        title: title.into(),
        kind: wire::SurfaceKind::Terminal { tmux_pane: Some("%1".into()), cols, rows },
    }
}

fn browser(id: &str, title: &str, url: &str) -> wire::Surface {
    wire::Surface {
        id: id.into(),
        title: title.into(),
        kind: wire::SurfaceKind::Browser {
            url: url.into(),
            loading: true,
            can_go_back: true,
            can_go_forward: false,
        },
    }
}

fn pane(id: &str, surfaces: Vec<wire::Surface>, active: &str) -> wire::Pane {
    wire::Pane { id: id.into(), surfaces, active_surface: Some(active.into()) }
}

fn sample_tree() -> wire::Tree {
    use wire::*;

    // focus = pane:1, three chips with the *middle* one active.
    let ws1 = Workspace {
        id: "workspace:1".into(),
        title: "claude · api-refactor".into(),
        folder: Some("folder:1".into()),
        cwd: "/Users/me/Documents/lmux".into(),
        git_branch: Some("feat/auth-refactor".into()),
        agent: AgentState::Running,
        summary: Some(
            "Refactored the auth middleware, wired the new token store, and started on the rate limiter. Next: integration tests."
                .into(),
        ),
        unread_notifications: 0,
        status_pills: vec![
            StatusPill { key: "model".into(), value: "opus".into(), icon: Some("◆".into()), color: Some("#c084fc".into()) },
            StatusPill { key: "tests".into(), value: "12 passing".into(), icon: Some("✓".into()), color: Some("#4ade80".into()) },
        ],
        progress: Some(Progress { value: 0.62, label: Some("auth-refactor".into()) }),
        layout: Layout::Pane { pane: "pane:1".into() },
        panes: vec![pane(
            "pane:1",
            vec![term("surface:1", "claude", 80, 24), term("surface:2", "npm test", 80, 24), term("surface:3", "htop", 80, 24)],
            "surface:2",
        )],
        focused_pane: Some("pane:1".into()),
        ports: vec![],
        pr: None,
    };

    // A single chip, one level down in a nested folder.
    let ws2 = Workspace {
        id: "workspace:2".into(),
        title: "tests · flaky suite".into(),
        folder: Some("folder:3".into()),
        cwd: "/Users/me/Documents/lmux".into(),
        git_branch: Some("main".into()),
        agent: AgentState::NeedsInput,
        summary: Some("Test `test_checkout_flow` failed twice in a row. Run it again or skip?".into()),
        unread_notifications: 2,
        status_pills: vec![StatusPill { key: "suite".into(), value: "3 failing".into(), icon: Some("✗".into()), color: Some("#f87171".into()) }],
        progress: None,
        layout: Layout::Pane { pane: "pane:2".into() },
        panes: vec![pane("pane:2", vec![term("surface:4", "zsh", 80, 24)], "surface:4")],
        focused_pane: Some("pane:2".into()),
        ports: vec![],
        pr: None,
    };

    // Terminal + browser split.
    let ws3 = Workspace {
        id: "workspace:3".into(),
        title: "docs · lmux site".into(),
        folder: Some("folder:2".into()),
        cwd: "/Users/me/Documents/lmux".into(),
        git_branch: Some("docs/site".into()),
        agent: AgentState::Idle,
        summary: Some("Docs build is green. Preview is live on port 3000.".into()),
        unread_notifications: 0,
        status_pills: vec![],
        progress: None,
        layout: Layout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.55,
            first: Box::new(Layout::Pane { pane: "pane:3".into() }),
            second: Box::new(Layout::Pane { pane: "pane:4".into() }),
        },
        panes: vec![
            pane("pane:3", vec![term("surface:5", "zsh", 80, 24)], "surface:5"),
            pane("pane:4", vec![browser("surface:6", "lmux.dev", "https://lmux.dev/docs")], "surface:6"),
        ],
        focused_pane: Some("pane:4".into()),
        ports: vec![],
        pr: None,
    };

    // Three panes: the focused one has three chips with the middle active, the
    // others have one and two chips respectively.
    let ws4 = Workspace {
        id: "workspace:4".into(),
        title: "review · three parallel".into(),
        folder: Some("folder:2".into()),
        cwd: "/Users/me/Documents/lmux".into(),
        git_branch: Some("main".into()),
        agent: AgentState::Running,
        summary: Some("Scaffold review and three parallel agents.".into()),
        unread_notifications: 1,
        status_pills: vec![StatusPill { key: "agents".into(), value: "3".into(), icon: Some("◆".into()), color: Some("#c084fc".into()) }],
        progress: None,
        layout: Layout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.4,
            first: Box::new(Layout::Pane { pane: "pane:5".into() }),
            second: Box::new(Layout::Split {
                direction: SplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(Layout::Pane { pane: "pane:6".into() }),
                second: Box::new(Layout::Pane { pane: "pane:7".into() }),
            }),
        },
        panes: vec![
            pane("pane:5", vec![term("surface:7", "core", 60, 20), term("surface:8", "logs", 60, 20)], "surface:7"),
            pane(
                "pane:6",
                vec![
                    browser("surface:9", "app", "https://example.com/"),
                    term("surface:10", "app logs", 60, 12),
                    term("surface:11", "worker", 60, 12),
                ],
                "surface:10",
            ),
            pane("pane:7", vec![term("surface:12", "shell", 60, 12)], "surface:12"),
        ],
        focused_pane: Some("pane:6".into()),
        ports: vec![],
        pr: None,
    };

    Tree {
        host: "my-server".into(),
        folders: vec![
            Folder { id: "folder:1".into(), name: "webapp".into(), parent: None, collapsed: false },
            // Nested one level: a folder inside a folder, holding a session.
            Folder { id: "folder:3".into(), name: "api".into(), parent: Some("folder:1".into()), collapsed: false },
            Folder { id: "folder:2".into(), name: "lintlabs".into(), parent: None, collapsed: false },
        ],
        workspaces: vec![ws1, ws2, ws3, ws4],
        selected_workspace: Some("workspace:1".into()),
    }
}

struct Fixture {
    state: AppState,
    net: Net,
    /// The harness runs one pass before we can install fonts; that first pass
    /// must not reference our named families.
    fonts_ready: bool,
}

fn fixture(selected: &str, with_terms: bool) -> Fixture {
    let mut state = AppState::new();
    state.apply_tree(sample_tree());
    state.selected = Some(selected.into());
    if with_terms {
        let mut t = TermView::new(80, 24);
        t.feed(b"\x1b[1;32mclaude\x1b[0m \x1b[90m~/Documents/lmux\x1b[0m\r\n");
        t.feed(b"$ cargo test\r\n");
        t.feed(b"\x1b[32m    Finished\x1b[0m test [unoptimized] target(s)\r\n");
        state.terms.insert("surface:1".into(), t);

        let mut t2 = TermView::new(80, 24);
        t2.feed(b"\x1b[1;34mnpm test\x1b[0m\r\n");
        t2.feed(b"\x1b[32m     Running\x1b[0m tests/api.rs\r\n");
        t2.feed(b"test api::auth ... \x1b[32mok\x1b[0m\r\n");
        t2.feed(b"\x1b[31mtest api::rate_limit ... FAILED\x1b[0m\r\n");
        t2.feed(b"$ ");
        state.terms.insert("surface:2".into(), t2);

        let mut t5 = TermView::new(80, 24);
        t5.feed(b"\x1b[36mzsh\x1b[0m \x1b[90m~/site\x1b[0m\r\n$ npm run dev\r\n");
        t5.feed(b"  \x1b[32mVITE\x1b[0m ready in 240 ms\r\n  \x1b[36m->\x1b[0m  Local: http://localhost:3000/\r\n");
        state.terms.insert("surface:5".into(), t5);

        let b = state.ensure_browser("surface:6", "https://lmux.dev/docs");
        b.loading = true;
        b.can_back = true;
    }
    // Both hosts the app offers: the daemon on this Mac, and the remote one the
    // sample tree belongs to.
    let mut net = Net::with_hosts(vec![
        HostConfig {
            name: "this mac".into(),
            label: "this mac".into(),
            kind: "direct".into(),
            url: Some("ws://127.0.0.1:7879".into()),
            ssh_target: None,
            remote_port: 7878,
            browser_scale: None,
            cloud: None,
        },
        HostConfig {
            name: "my-server".into(),
            label: "my-server".into(),
            kind: "direct".into(),
            url: Some("ws://127.0.0.1:7900".into()),
            ssh_target: None,
            remote_port: 7878,
            browser_scale: None,
            cloud: None,
        },
    ]);
    net.host_index = 1;
    net.state = ConnState::Connected;
    state.set_current_host("my-server");
    Fixture { state, net, fonts_ready: false }
}

fn harness<'a>(fx: Fixture, size: Vec2) -> Harness<'a, Fixture> {
    let mut h = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(2.0)
        .with_max_steps(8)
        .build_ui_state(
            |ui, fx: &mut Fixture| {
                if !fx.fonts_ready {
                    fonts::install(ui.ctx());
                    fx.fonts_ready = true;
                    return;
                }
                let mut c = lmux_app::ui::Ctx { state: &mut fx.state, net: &mut fx.net };
                app::render(ui, &mut c);
            },
            fx,
        );
    h.run();
    h
}

#[test]
fn snapshot_sidebar_and_cards() {
    let mut h = harness(fixture("workspace:1", true), Vec2::new(560.0, 620.0));
    h.snapshot("sidebar");
}

/// The empty sidebar's menu: every entry names the folder it targets, so the
/// nested folder shows up as `webapp / api`.
#[test]
fn snapshot_sidebar_new_item_menu() {
    let mut h = harness(fixture("workspace:3", true), Vec2::new(560.0, 620.0));
    // Empty sidebar below the last card (the body ends above the status bar).
    let below = Pos2::new(100.0, 580.0);
    h.input_mut().events.push(egui::Event::PointerMoved(below));
    h.run();
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos: below,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::default(),
        });
        h.run();
    }
    h.snapshot("sidebar_menu");
}

/// A folder row's menu: everything in it acts on that folder, including
/// nesting another folder inside it.
#[test]
fn snapshot_sidebar_folder_menu() {
    let mut h = harness(fixture("workspace:1", true), Vec2::new(560.0, 620.0));
    let row = *h.state().state.folder_rects.get("folder:1").expect("folder row rect");
    let at = row.center();
    h.input_mut().events.push(egui::Event::PointerMoved(at));
    h.run();
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::default(),
        });
        h.run();
    }
    h.snapshot("sidebar_folder_menu");
}

/// Double clicking a folder row renames it in place: the row swaps its label
/// for an editor and keeps the collapse state it had.
#[test]
fn snapshot_sidebar_folder_rename() {
    let mut h = harness(fixture("workspace:1", true), Vec2::new(560.0, 620.0));
    let row = *h.state().state.folder_rects.get("folder:1").expect("folder row rect");
    let at = row.center();
    h.input_mut().events.push(egui::Event::PointerMoved(at));
    h.step();
    for _ in 0..2 {
        for pressed in [true, false] {
            h.input_mut().events.push(egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Modifiers::default(),
            });
        }
    }
    h.step();
    h.run_steps(2);
    assert!(h.state().state.folder_rename.is_some(), "the row is editing");
    h.snapshot("sidebar_folder_rename");
}

#[test]
fn snapshot_split_layout() {
    let mut h = harness(fixture("workspace:4", true), Vec2::new(900.0, 560.0));
    h.snapshot("split");
}

#[test]
fn snapshot_terminal_grid() {
    let mut h = harness(fixture("workspace:3", true), Vec2::new(820.0, 460.0));
    h.snapshot("terminal");
}

/// A mouse selection (translucent wash under the text) and, with Cmd held,
/// the URL under the pointer underlined.
#[test]
fn snapshot_terminal_selection_and_link() {
    let mut fx = fixture("workspace:3", true);
    if let Some(t) = fx.state.terms.get_mut("surface:5") {
        t.start_selection(lmux_app::term::SelectionType::Simple, 1, 0, false);
        t.update_selection(2, 8, true);
    }
    let mut h = harness(fx, Vec2::new(820.0, 460.0));
    let (origin, cell) = {
        let t = &h.state().state.terms["surface:5"];
        (t.grid_origin, t.cell_size)
    };
    assert!(origin != Pos2::ZERO, "surface:5 is the terminal on screen");
    let cmd = Modifiers { command: true, mac_cmd: true, ..Default::default() };
    h.input_mut().events.push(egui::Event::ModifiersChanged(cmd));
    h.input_mut().events.push(egui::Event::PointerMoved(origin + Vec2::new(cell.x * 22.5, cell.y * 3.5)));
    h.run();
    h.snapshot("terminal_selection_link");
}

#[test]
fn snapshot_narrow_window() {
    let mut h = harness(fixture("workspace:4", true), Vec2::new(460.0, 600.0));
    h.snapshot("narrow");
}

#[test]
fn snapshot_disconnected_banner() {
    let mut fx = fixture("workspace:1", true);
    fx.net.state = ConnState::Disconnected;
    fx.net.last_error = Some("connection refused".into());
    let mut h = harness(fx, Vec2::new(900.0, 460.0));
    h.snapshot("disconnected");
}

#[test]
fn snapshot_notifications_panel() {
    let mut fx = fixture("workspace:1", true);
    fx.state.notifications.push(wire::Notification {
        id: "notif:1".into(),
        title: "Claude finished a task".into(),
        subtitle: Some("claude · api-refactor".into()),
        body: Some("Rate limiter done; 12 tests passing.".into()),
        workspace: Some("workspace:1".into()),
        surface: None,
        created_at: 0,
        read: false,
    });
    fx.state.notif_open = true;
    let mut h = harness(fx, Vec2::new(900.0, 560.0));
    h.snapshot("notifications");
}

/// Terminal content with fallback-font glyphs (Claude Code's `⏺ ✻`, emoji, CJK)
/// and column probes on the right: `abcdef|` prefixed by a space must line up
/// with one prefixed by `⏺`; two spaces must line up with one prefixed by `漢`.
#[test]
fn snapshot_terminal_glyphs() {
    let mut fx = fixture("workspace:1", false);
    {
        let ws = &mut fx.state.tree.workspaces[0];
        ws.panes[0].active_surface = Some("surface:1".into());
    }
    let mut t = TermView::new(80, 24);
    t.feed(" abcdef|\r\n".as_bytes());
    t.feed("\u{23FA}abcdef|\r\n".as_bytes());
    t.feed("  abcdef|\r\n".as_bytes());
    t.feed("\u{6F22}abcdef|\r\n".as_bytes());
    t.feed("claude \u{23FA} Read \u{273B} Thinking \u{1F680} \u{6F22}\u{5B57} done\r\n".as_bytes());
    fx.state.terms.insert("surface:1".into(), t);
    let mut h = harness(fx, Vec2::new(760.0, 340.0));
    h.snapshot("terminal_glyphs");
}

/// A browser pane showing a decoded frame, scaled up to the pane (review #9:
/// the stream is 1×, so the frame is stretched to the pane's CSS size).
#[test]
fn snapshot_browser_frame() {
    let mut fx = fixture("workspace:3", true);
    let jpeg = include_bytes!("data/frame1280.jpg");
    let (image, w, h) = lmux_app::decode::decode_jpeg(jpeg).expect("decode frame");
    let b = fx.state.ensure_browser("surface:6", "https://lmux.dev/docs");
    b.pending_image = Some(image);
    b.frame_size = (w, h);
    b.have_frame = true;
    let mut h = harness(fx, Vec2::new(900.0, 520.0));
    h.snapshot("browser_frame");
}

/// Tab strip, single-chip pane.
#[test]
fn snapshot_tabs_single_chip() {
    let mut h = harness(fixture("workspace:2", true), Vec2::new(760.0, 300.0));
    h.snapshot("tabs_single_chip");
}

/// Tab strip, several chips with the active one *not* last, in a focused pane
/// next to unfocused panes that have their own strips.
#[test]
fn snapshot_tabs_focused_and_unfocused() {
    let mut h = harness(fixture("workspace:4", true), Vec2::new(1000.0, 420.0));
    h.snapshot("tabs_focused_unfocused");
}

// --- in-drag visuals --------------------------------------------------------

/// A session dragged onto the sidebar, aimed between two cards: the accent line
/// sits at the gap the daemon will insert into, and the dragged card is dimmed.
#[test]
fn snapshot_sidebar_drag_between_cards() {
    let mut h = harness(fixture("workspace:1", true), Vec2::new(900.0, 560.0));
    let from = card_rect(&h, "workspace:2");
    let onto = card_rect(&h, "workspace:3");
    let grab = Pos2::new(from.min.x + 30.0, from.min.y + 12.0);
    let to = Pos2::new(onto.center().x, onto.min.y + 4.0);
    press_drag(&mut h, grab, to);
    h.snapshot("sidebar_drag_between");
}

/// A folder dragged onto another folder row: the row is filled, because the
/// folder is about to go inside it rather than between two rows.
#[test]
fn snapshot_sidebar_drag_into_folder() {
    let mut h = harness(fixture("workspace:1", true), Vec2::new(900.0, 560.0));
    let from = *h
        .state()
        .state
        .folder_rects
        .get("folder:2")
        .expect("folder:2 row");
    let onto = *h
        .state()
        .state
        .folder_rects
        .get("folder:1")
        .expect("folder:1 row");
    press_drag(&mut h, from.center(), onto.center());
    h.snapshot("sidebar_drag_into_folder");
}

fn card_rect(h: &Harness<Fixture>, workspace: &str) -> egui::Rect {
    *h.state()
        .state
        .card_rects
        .get(workspace)
        .unwrap_or_else(|| panic!("no card rect for {workspace}"))
}

/// The controls a card reveals on hover: the `×` that closes that session, set
/// beside the unread badge rather than under it.
#[test]
fn snapshot_sidebar_card_close_button() {
    let mut h = harness(fixture("workspace:1", true), Vec2::new(900.0, 560.0));
    // workspace:2 carries two unread notifications, so the `×` has to make room.
    let card = card_rect(&h, "workspace:2");
    let hover = Pos2::new(card.min.x + 40.0, card.min.y + 11.0);
    h.input_mut().events.push(egui::Event::PointerMoved(hover));
    h.step();
    h.step();
    h.snapshot("sidebar_card_close");
}

fn chip_rect(h: &Harness<Fixture>, surface: &str) -> egui::Rect {
    *h.state()
        .state
        .chip_rects
        .get(surface)
        .unwrap_or_else(|| panic!("no chip rect for {surface}"))
}

fn pane_rect(h: &Harness<Fixture>, pane: &str) -> egui::Rect {
    *h.state()
        .state
        .pane_rects
        .get(pane)
        .unwrap_or_else(|| panic!("no pane rect for {pane}"))
}

/// Press on `from` and move to `to`, one explicit `step` per pointer event.
/// `run` cannot be used: while the button is down the drag requests a repaint
/// every frame, so `run` would step until it hits `max_steps`.
fn press_drag(h: &mut Harness<Fixture>, from: Pos2, to: Pos2) {
    h.input_mut().events.push(egui::Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(egui::Event::PointerButton {
        pos: from,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::default(),
    });
    h.step();
    for t in [0.5f32, 1.0] {
        let p = Pos2::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
        h.input_mut().events.push(egui::Event::PointerMoved(p));
        h.step();
    }
}

fn chip_grab(r: egui::Rect) -> Pos2 {
    Pos2::new(r.min.x + 26.0, r.center().y)
}

/// Reordering inside a strip: the insertion caret at the drop slot, the ghost
/// under the pointer, and the source chip dimmed.
#[test]
fn snapshot_drag_reorder_caret() {
    let mut h = harness(fixture("workspace:4", true), Vec2::new(1000.0, 460.0));
    let s7 = chip_rect(&h, "surface:7");
    let s8 = chip_rect(&h, "surface:8");
    let to = Pos2::new(s8.max.x - 3.0, s8.center().y);
    press_drag(&mut h, chip_grab(s7), to);
    h.snapshot("drag_reorder");
}

/// Moving a chip onto another pane's body: the target pane is highlighted.
#[test]
fn snapshot_drag_pane_highlight() {
    let mut h = harness(fixture("workspace:4", true), Vec2::new(1000.0, 460.0));
    let s12 = chip_rect(&h, "surface:12");
    let p5 = pane_rect(&h, "pane:5");
    press_drag(&mut h, chip_grab(s12), p5.center());
    h.snapshot("drag_target_pane");
}

/// An edge drop: the highlighted pane and the half that a split would take.
#[test]
fn snapshot_drag_edge_indicator() {
    let mut h = harness(fixture("workspace:4", true), Vec2::new(1000.0, 460.0));
    let s7 = chip_rect(&h, "surface:7");
    let p5 = pane_rect(&h, "pane:5");
    let to = Pos2::new(p5.max.x - 20.0, p5.center().y);
    press_drag(&mut h, chip_grab(s7), to);
    h.snapshot("drag_edge_split");
}

/// A terminal with history shows a thin scrollbar; scrolled back, it sits
/// partway up and brightens.
#[test]
fn snapshot_terminal_scrollbar() {
    let mut fx = fixture("workspace:1", false);
    {
        let ws = &mut fx.state.tree.workspaces[0];
        ws.panes[0].active_surface = Some("surface:1".into());
    }
    let mut t = TermView::new(80, 24);
    for i in 0..300 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    t.scroll(120);
    fx.state.terms.insert("surface:1".into(), t);
    let mut h = harness(fx, Vec2::new(900.0, 500.0));
    h.snapshot("terminal_scrollbar");
}
