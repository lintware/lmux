//! Drag-and-drop gesture tests: press → move → release driven with real egui
//! pointer events against the geometry the UI actually drew (`AppState::chip_rects`
//! and `AppState::pane_rects`, recorded by the render pass). Each test asserts the
//! RPC that is issued (method + params), and the positive ones then feed the tree
//! the daemon would send back and assert the resulting state.

use egui::{Modifiers, Pos2, Vec2};
use egui_kittest::Harness;
use lmux_app::net::{ConnState, HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::wire::{self, ClientMsg};

fn term(id: &str, title: &str) -> wire::Surface {
    wire::Surface {
        id: id.into(),
        title: title.into(),
        kind: wire::SurfaceKind::Terminal { tmux_pane: None, cols: 80, rows: 24 },
    }
}

fn pane(id: &str, surfaces: Vec<wire::Surface>, active: &str) -> wire::Pane {
    wire::Pane { id: id.into(), surfaces, active_surface: Some(active.into()) }
}

/// Two panes side by side: `pane:1` holds `surface:1`,`surface:2`; `pane:2`
/// holds `surface:3`. Focus is `pane:1`.
fn two_pane_tree() -> wire::Tree {
    use wire::*;
    let ws = Workspace {
        id: "workspace:1".into(),
        title: "drag".into(),
        folder: None,
        cwd: "/tmp".into(),
        git_branch: None,
        agent: AgentState::None,
        summary: None,
        unread_notifications: 0,
        status_pills: vec![],
        progress: None,
        layout: Layout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(Layout::Pane { pane: "pane:1".into() }),
            second: Box::new(Layout::Pane { pane: "pane:2".into() }),
        },
        panes: vec![
            pane("pane:1", vec![term("surface:1", "one"), term("surface:2", "two")], "surface:1"),
            pane("pane:2", vec![term("surface:3", "three")], "surface:3"),
        ],
        focused_pane: Some("pane:1".into()),
        ports: vec![],
        pr: None,
    };
    Tree {
        host: "h".into(),
        folders: vec![],
        workspaces: vec![ws],
        selected_workspace: Some("workspace:1".into()),
    }
}

struct Fx {
    state: AppState,
    net: Net,
    ready: bool,
}

fn fixture() -> Fx {
    let mut state = AppState::new();
    state.apply_tree(two_pane_tree());
    state.selected = Some("workspace:1".into());
    let net = Net::with_hosts(vec![HostConfig {
        name: "mock".into(),
        label: "mock".into(),
        kind: "direct".into(),
        url: Some("ws://127.0.0.1:7900".into()),
        ssh_target: None,
        remote_port: 7878,
        browser_scale: None,
        stream_browsers: false,
        cloud: None,
    }]);
    Fx { state, net, ready: false }
}

fn harness<'a>(fx: Fx) -> Harness<'a, Fx> {
    let mut h = Harness::builder()
        .with_size(Vec2::new(900.0, 560.0))
        .with_pixels_per_point(1.0)
        .with_max_steps(8)
        .build_ui_state(
            |ui, fx: &mut Fx| {
                if !fx.ready {
                    lmux_app::fonts::install(ui.ctx());
                    fx.ready = true;
                    return;
                }
                let mut c = lmux_app::ui::Ctx { state: &mut fx.state, net: &mut fx.net };
                lmux_app::app::render(ui, &mut c);
            },
            fx,
        );
    h.run();
    h.run();
    h
}

fn chip_rect(h: &Harness<Fx>, surface: &str) -> egui::Rect {
    *h.state()
        .state
        .chip_rects
        .get(surface)
        .unwrap_or_else(|| panic!("no chip rect for {surface}"))
}

fn pane_rect(h: &Harness<Fx>, pane: &str) -> egui::Rect {
    *h.state()
        .state
        .pane_rects
        .get(pane)
        .unwrap_or_else(|| panic!("no pane rect for {pane}"))
}

fn rpcs(h: &Harness<Fx>) -> Vec<(String, serde_json::Value)> {
    h.state()
        .state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::Rpc { request } => Some((request.method.clone(), request.params.clone())),
            _ => None,
        })
        .collect()
}

fn find(h: &Harness<Fx>, method: &str) -> Option<serde_json::Value> {
    rpcs(h).into_iter().rev().find(|(m, _)| m == method).map(|(_, p)| p)
}

/// Press at `from`, move through intermediate points to `to`, release.
///
/// While the button is down the UI keeps requesting repaints (the ghost must
/// follow the pointer), so `Harness::run` — which steps until the repaint queue
/// settles — would spin forever. Each pointer step is therefore one explicit
/// `step`; `run` returns once the drag has cleared.
fn drag(h: &mut Harness<Fx>, from: Pos2, to: Pos2) {
    h.input_mut().events.push(egui::Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(egui::Event::PointerButton {
        pos: from,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::default(),
    });
    h.step();
    for t in [0.25f32, 0.5, 0.75, 1.0] {
        let p = Pos2::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
        h.input_mut().events.push(egui::Event::PointerMoved(p));
        h.step();
    }
    h.input_mut().events.push(egui::Event::PointerButton {
        pos: to,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::default(),
    });
    h.step();
    h.step();
}

/// A point on the chip's title area (never the close button).
fn chip_grab(r: egui::Rect) -> Pos2 {
    Pos2::new(r.min.x + 26.0, r.center().y)
}

fn no_move_rpcs(h: &Harness<Fx>) -> bool {
    let r = rpcs(h);
    !r.iter().any(|(m, _)| {
        m == "surface.move" || m == "surface.split" || m == "pane.swap"
    })
}

/// A sidebar with two folders and two top-level sessions, each session a single
/// terminal. The daemon's list order is `w1`(in `f1`), `w2`(in `f2`), `w3`,
/// `w4`, and the sidebar draws the folders first, so the cards come out as
/// `w1`, `w2`, `w3`, `w4`.
fn sidebar_tree() -> wire::Tree {
    use wire::*;
    let ws = |id: &str, title: &str, folder: Option<&str>, surf: &str| Workspace {
        id: id.into(),
        title: title.into(),
        folder: folder.map(str::to_owned),
        cwd: "/tmp".into(),
        git_branch: None,
        agent: AgentState::None,
        summary: None,
        unread_notifications: 0,
        status_pills: vec![],
        progress: None,
        layout: Layout::Pane { pane: format!("pane:{id}") },
        panes: vec![pane(&format!("pane:{id}"), vec![term(surf, title)], surf)],
        focused_pane: Some(format!("pane:{id}")),
        ports: vec![],
        pr: None,
    };
    Tree {
        host: "h".into(),
        folders: vec![
            Folder { id: "folder:1".into(), name: "webapp".into(), parent: None, collapsed: false },
            Folder { id: "folder:2".into(), name: "lintlabs".into(), parent: None, collapsed: false },
        ],
        workspaces: vec![
            ws("workspace:1", "one", Some("folder:1"), "surface:1"),
            ws("workspace:2", "two", Some("folder:2"), "surface:2"),
            ws("workspace:3", "three", None, "surface:3"),
            ws("workspace:4", "four", None, "surface:4"),
        ],
        selected_workspace: Some("workspace:3".into()),
    }
}

fn sidebar_fixture() -> Fx {
    let mut state = AppState::new();
    state.apply_tree(sidebar_tree());
    state.selected = Some("workspace:3".into());
    let net = Net::with_hosts(vec![HostConfig {
        name: "mock".into(),
        label: "mock".into(),
        kind: "direct".into(),
        url: Some("ws://127.0.0.1:7900".into()),
        ssh_target: None,
        remote_port: 7878,
        browser_scale: None,
        stream_browsers: false,
        cloud: None,
    }]);
    Fx { state, net, ready: false }
}

fn card_rect(h: &Harness<Fx>, workspace: &str) -> egui::Rect {
    *h.state()
        .state
        .card_rects
        .get(workspace)
        .unwrap_or_else(|| panic!("no card rect for {workspace}"))
}

fn folder_row(h: &Harness<Fx>, folder: &str) -> egui::Rect {
    *h.state()
        .state
        .folder_rects
        .get(folder)
        .unwrap_or_else(|| panic!("no folder row for {folder}"))
}

/// A point on a card's title, away from the edges.
fn card_grab(r: egui::Rect) -> Pos2 {
    Pos2::new(r.min.x + 30.0, r.min.y + 10.0)
}

/// A point in the sidebar's empty tail: below every row, above the status bar.
fn below_the_rows(h: &Harness<Fx>) -> Pos2 {
    let bottom = h
        .state()
        .state
        .sidebar_rows
        .iter()
        .map(|r| r.rect.max.y)
        .fold(0.0, f32::max);
    Pos2::new(100.0, (bottom + 12.0).min(520.0))
}

// --- sidebar rows -----------------------------------------------------------

#[test]
fn dragging_a_session_onto_a_folder_row_moves_it_into_that_folder() {
    let mut h = harness(sidebar_fixture());
    let card = card_rect(&h, "workspace:3");
    let row = folder_row(&h, "folder:1");
    drag(&mut h, card_grab(card), row.center());

    let call = find(&h, "workspace.move").expect("workspace.move must be sent");
    assert_eq!(call["workspace"], "workspace:3");
    assert_eq!(call["folder"], "folder:1");
    assert_eq!(call["index"], 0, "it lands in front of the folder's own session");
    assert!(h.state().state.tab_drag.is_none(), "the drag must clear on release");
}

#[test]
fn dragging_a_session_out_to_the_empty_tail_returns_it_to_the_top_level() {
    let mut h = harness(sidebar_fixture());
    let card = card_rect(&h, "workspace:1");
    let to = below_the_rows(&h);
    drag(&mut h, card_grab(card), to);

    let call = find(&h, "workspace.move").expect("workspace.move must be sent");
    assert_eq!(call["workspace"], "workspace:1");
    assert!(call["folder"].is_null(), "the empty tail is the top level");
    assert!(call.get("index").is_none(), "appending leaves the index out");
}

#[test]
fn dragging_a_session_onto_another_session_reorders_it() {
    let mut h = harness(sidebar_fixture());
    let four = card_rect(&h, "workspace:4");
    let three = card_rect(&h, "workspace:3");
    // The upper half of `three` puts `four` in front of it.
    let to = Pos2::new(three.center().x, three.min.y + 4.0);
    drag(&mut h, card_grab(four), to);

    let call = find(&h, "workspace.move").expect("workspace.move must be sent");
    assert_eq!(call["workspace"], "workspace:4");
    assert!(call["folder"].is_null());
    // Without `four` the list is [one, two, three], so `three` is at index 2.
    assert_eq!(call["index"], 2);
}

#[test]
fn dragging_a_session_onto_the_row_it_already_holds_sends_nothing() {
    let mut h = harness(sidebar_fixture());
    let three = card_rect(&h, "workspace:3");
    let four = card_rect(&h, "workspace:4");
    // `three` sits directly above `four`, so dropping it on `four`'s top half
    // is where it already is.
    let to = Pos2::new(four.center().x, four.min.y + 4.0);
    drag(&mut h, card_grab(three), to);

    assert!(
        find(&h, "workspace.move").is_none(),
        "a drop into its own slot must not issue an RPC"
    );
    assert!(h.state().state.tab_drag.is_none());
}

#[test]
fn dragging_a_folder_onto_another_folder_row_nests_it() {
    let mut h = harness(sidebar_fixture());
    let from = folder_row(&h, "folder:2");
    let onto = folder_row(&h, "folder:1");
    drag(&mut h, Pos2::new(from.center().x, from.center().y), onto.center());

    let call = find(&h, "folder.move").expect("folder.move must be sent");
    assert_eq!(call["folder"], "folder:2");
    assert_eq!(call["parent"], "folder:1");
    assert!(call.get("index").is_none(), "the only child lands at the end");
}

#[test]
fn dragging_a_folder_to_the_empty_tail_lifts_it_to_the_top_level() {
    let mut h = harness(sidebar_fixture());
    let from = folder_row(&h, "folder:2");
    let to = below_the_rows(&h);
    drag(&mut h, Pos2::new(from.center().x, from.center().y), to);

    let call = find(&h, "folder.move").expect("folder.move must be sent");
    assert_eq!(call["folder"], "folder:2");
    assert!(call["parent"].is_null(), "the empty tail is the top level");
}

// --- positive gestures ------------------------------------------------------

#[test]
fn reorder_within_a_strip_issues_surface_move_with_index() {
    let mut h = harness(fixture());
    let s1 = chip_rect(&h, "surface:1");
    let s2 = chip_rect(&h, "surface:2");
    // Drop just right of s2's centre: after removing surface:1 the list is
    // [surface:2], so the slot is index 1 (the end).
    let to = Pos2::new(s2.max.x - 3.0, s2.center().y);
    drag(&mut h, chip_grab(s1), to);

    let call = find(&h, "surface.move").expect("surface.move must be sent");
    assert_eq!(call["surface"], "surface:1");
    assert_eq!(call["pane"], "pane:1");
    assert_eq!(call["index"], 1);
    assert!(h.state().state.tab_drag.is_none(), "the drag must clear on release");

    // The daemon would send the reordered tree back; render from it.
    h.state_mut().state.apply_tree({
        let mut t = two_pane_tree();
        t.workspaces[0].panes[0].surfaces = vec![term("surface:2", "two"), term("surface:1", "one")];
        t
    });
    h.run();
    let order: Vec<String> = h
        .state()
        .state
        .pane("pane:1")
        .unwrap()
        .surfaces
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(order, vec!["surface:2", "surface:1"]);
}

#[test]
fn move_to_another_pane_issues_surface_move_without_index() {
    let mut h = harness(fixture());
    let s1 = chip_rect(&h, "surface:1");
    let p2 = pane_rect(&h, "pane:2");
    // The pane body, away from the edges so it is a plain move, not a split.
    let to = p2.center();
    drag(&mut h, chip_grab(s1), to);

    let call = find(&h, "surface.move").expect("surface.move must be sent");
    assert_eq!(call["surface"], "surface:1");
    assert_eq!(call["pane"], "pane:2");
    assert!(call.get("index").is_none(), "a body drop appends: no index");

    // The daemon sends the moved tree back.
    h.state_mut().state.apply_tree({
        let mut t = two_pane_tree();
        t.workspaces[0].panes[0].surfaces = vec![term("surface:2", "two")];
        t.workspaces[0].panes[1].surfaces =
            vec![term("surface:3", "three"), term("surface:1", "one")];
        t.workspaces[0].panes[1].active_surface = Some("surface:1".into());
        t
    });
    h.run();
    assert_eq!(h.state().state.pane_of_surface("surface:1").as_deref(), Some("pane:2"));
}

#[test]
fn edge_drop_issues_surface_split_with_the_matching_side() {
    let mut h = harness(fixture());
    let s1 = chip_rect(&h, "surface:1");
    let p1 = pane_rect(&h, "pane:1");
    // The outer-left band of pane:1: 20 px in, well within 25%.
    let to = Pos2::new(p1.min.x + 20.0, p1.center().y);
    drag(&mut h, chip_grab(s1), to);

    let call = find(&h, "surface.split").expect("surface.split must be sent");
    assert_eq!(call["surface"], "surface:1");
    assert_eq!(call["side"], "left");
    assert_eq!(call["pane"], "pane:1", "the split names the pane under the pointer");

    // The daemon moved the surface out into a new pane on the left.
    h.state_mut().state.apply_tree({
        let mut t = two_pane_tree();
        let ws = &mut t.workspaces[0];
        ws.panes[0].surfaces = vec![term("surface:2", "two")];
        ws.panes.push(pane("pane:3", vec![term("surface:1", "one")], "surface:1"));
        ws.layout = wire::Layout::Split {
            direction: wire::SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(wire::Layout::Pane { pane: "pane:3".into() }),
            second: Box::new(wire::Layout::Pane { pane: "pane:1".into() }),
        };
        t
    });
    h.run();
    assert_eq!(h.state().state.pane_of_surface("surface:1").as_deref(), Some("pane:3"));
    let rects = &h.state().state.pane_rects;
    assert!(
        rects["pane:3"].min.x < rects["pane:1"].min.x,
        "the split-off pane must be on the left"
    );
}

#[test]
fn edge_drop_on_another_pane_splits_that_pane_not_the_source() {
    let mut h = harness(fixture());
    let s1 = chip_rect(&h, "surface:1");
    let p2 = pane_rect(&h, "pane:2");
    // pane:1's tab dropped on pane:2's inner-left band.
    let to = Pos2::new(p2.min.x + 10.0, p2.center().y);
    drag(&mut h, chip_grab(s1), to);

    let call = find(&h, "surface.split").expect("surface.split must be sent");
    assert_eq!(call["surface"], "surface:1");
    assert_eq!(call["side"], "left");
    assert_eq!(
        call["pane"], "pane:2",
        "the split must come off the pane under the pointer, not the tab's own pane"
    );

    // The daemon splits pane:2; the tab lands in the new pane to its left.
    h.state_mut().state.apply_tree({
        let mut t = two_pane_tree();
        let ws = &mut t.workspaces[0];
        ws.panes[0].surfaces = vec![term("surface:2", "two")];
        ws.panes.push(pane("pane:4", vec![term("surface:1", "one")], "surface:1"));
        ws.layout = wire::Layout::Split {
            direction: wire::SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(wire::Layout::Pane { pane: "pane:1".into() }),
            second: Box::new(wire::Layout::Split {
                direction: wire::SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(wire::Layout::Pane { pane: "pane:4".into() }),
                second: Box::new(wire::Layout::Pane { pane: "pane:2".into() }),
            }),
        };
        t
    });
    h.run();
    assert_eq!(h.state().state.pane_of_surface("surface:1").as_deref(), Some("pane:4"));
    let rects = &h.state().state.pane_rects;
    assert!(
        rects["pane:4"].min.x < rects["pane:2"].min.x
            && rects["pane:4"].min.x > rects["pane:1"].min.x,
        "the new pane must sit between pane:1 and pane:2"
    );
}

#[test]
fn dragging_a_pane_header_onto_another_pane_issues_swap() {
    let mut h = harness(fixture());
    let p1 = pane_rect(&h, "pane:1");
    let p2 = pane_rect(&h, "pane:2");
    // The strip's empty padding, left of the first chip.
    let from = Pos2::new(p1.min.x + 2.0, p1.min.y + 11.0);
    drag(&mut h, from, p2.center());

    let call = find(&h, "pane.swap").expect("pane.swap must be sent");
    assert_eq!(call["pane"], "pane:1");
    assert_eq!(call["other"], "pane:2");

    // Swap the leaves; the panes' contents follow their ids to the new slots.
    h.state_mut().state.apply_tree({
        let mut t = two_pane_tree();
        t.workspaces[0].layout = wire::Layout::Split {
            direction: wire::SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(wire::Layout::Pane { pane: "pane:2".into() }),
            second: Box::new(wire::Layout::Pane { pane: "pane:1".into() }),
        };
        t
    });
    h.run();
    let rects = &h.state().state.pane_rects;
    assert!(
        rects["pane:2"].min.x < rects["pane:1"].min.x,
        "after the swap pane:2 must sit on the left"
    );
}

// --- negative cases: no RPC, drag cleared -----------------------------------

#[test]
fn dropping_on_the_source_chip_issues_nothing() {
    let mut h = harness(fixture());
    let s1 = chip_rect(&h, "surface:1");
    let grab = chip_grab(s1);
    // Drag away far enough to start, then come back and release on the source.
    drag(&mut h, grab, Pos2::new(s1.max.x - 20.0, s1.center().y));
    assert!(no_move_rpcs(&h), "dropping on the source chip must be a no-op");
    assert!(h.state().state.tab_drag.is_none());
}

#[test]
fn dropping_at_the_same_index_issues_nothing() {
    let mut h = harness(fixture());
    let s2 = chip_rect(&h, "surface:2");
    let grab = chip_grab(s2);
    // surface:2 is already last; releasing at its own slot resolves to index 1.
    drag(&mut h, grab, Pos2::new(s2.max.x - 4.0, s2.center().y));
    assert!(no_move_rpcs(&h), "an unchanged slot must be a no-op");
}

#[test]
fn dropping_on_an_empty_area_issues_nothing() {
    let mut h = harness(fixture());
    let s1 = chip_rect(&h, "surface:1");
    // The sidebar is not part of any pane.
    let to = Pos2::new(100.0, 300.0);
    drag(&mut h, chip_grab(s1), to);
    assert!(no_move_rpcs(&h), "a drop outside every pane must be a no-op");
    assert!(h.state().state.tab_drag.is_none());
}

#[test]
fn releasing_outside_any_pane_issues_nothing() {
    let mut h = harness(fixture());
    let s1 = chip_rect(&h, "surface:1");
    let p1 = pane_rect(&h, "pane:1");
    // The workspace header row sits above every pane.
    let to = Pos2::new(p1.center().x, p1.min.y - 10.0);
    drag(&mut h, chip_grab(s1), to);
    assert!(no_move_rpcs(&h), "a release on the header must be a no-op");
}

#[test]
fn clicking_a_chip_still_selects_rather_than_dragging() {
    let mut h = harness(fixture());
    let s2 = chip_rect(&h, "surface:2");
    let p = chip_grab(s2);
    h.input_mut().events.push(egui::Event::PointerMoved(p));
    h.run();
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        });
        h.run();
    }
    assert!(no_move_rpcs(&h), "a plain click must not start a drag");
    assert_eq!(
        h.state().state.pane_active.get("pane:1").map(String::as_str),
        Some("surface:2"),
        "a plain click still selects the chip"
    );
    let _ = ConnState::Connected;
}

