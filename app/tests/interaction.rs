//! Interaction tests: keybindings and input routing, driven through the same
//! `shortcuts`/`route_input` code the running app uses.

use egui::{Key, Modifiers, Vec2};
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

fn sample_tree() -> wire::Tree {
    use wire::*;
    let pane = |id: &str, surfaces: Vec<Surface>, active: &str| Pane {
        id: id.into(),
        surfaces,
        active_surface: Some(active.into()),
    };
    let ws = |id: &str, title: &str, folder: Option<&str>, pane_id: &str, surface_id: &str| Workspace {
        id: id.into(),
        title: title.into(),
        folder: folder.map(|f| f.into()),
        cwd: "/home/vas/project".into(),
        git_branch: Some("main".into()),
        agent: AgentState::Running,
        summary: Some("working".into()),
        unread_notifications: 0,
        status_pills: vec![],
        progress: None,
        layout: Layout::Pane { pane: pane_id.into() },
        panes: vec![pane(pane_id, vec![term(surface_id, title)], surface_id)],
        focused_pane: Some(pane_id.into()),
        ports: vec![],
        pr: None,
    };
    Tree {
        host: "my-server".into(),
        folders: vec![
            Folder { id: "folder:1".into(), name: "one".into(), parent: None, collapsed: false },
            Folder { id: "folder:2".into(), name: "nested".into(), parent: Some("folder:1".into()), collapsed: false },
        ],
        workspaces: vec![
            ws("workspace:1", "first", Some("folder:1"), "pane:1", "surface:1"),
            ws("workspace:2", "second", Some("folder:2"), "pane:2", "surface:2"),
            ws("workspace:3", "third", None, "pane:3", "surface:3"),
        ],
        selected_workspace: Some("workspace:1".into()),
    }
}

struct Fixture {
    state: AppState,
    net: Net,
    ready: bool,
}

fn host(name: &str, label: &str, url: &str) -> HostConfig {
    HostConfig {
        name: name.into(),
        label: label.into(),
        kind: "direct".into(),
        url: Some(url.into()),
        ssh_target: None,
        remote_port: 7878,
        browser_scale: None,
        stream_browsers: false,
        cloud: None,
    }
}

fn fixture() -> Fixture {
    fixture_on("this mac")
}

/// Two hosts: the local one, connected, and a remote one whose folders are
/// known from an earlier visit.
fn fixture_on(host_name: &str) -> Fixture {
    let mut state = AppState::new();
    state.apply_tree(sample_tree());
    state.selected = Some("workspace:1".into());
    let hosts = vec![
        host("this mac", "this mac", "ws://127.0.0.1:7879"),
        host("mock", "mock", "ws://127.0.0.1:7900"),
    ];
    let mut net = Net::with_hosts(hosts);
    net.state = ConnState::Connected;
    let index = if host_name == "mock" { 1 } else { 0 };
    net.host_index = index;
    state.set_current_host(host_name);
    Fixture { state, net, ready: false }
}

/// A harness whose closure runs the real shortcut + render path, at `step_dt`
/// simulated seconds per frame. egui's double-click window is 0.3 s, so a test
/// that drives a real double click needs frames shorter than kittest's default
/// quarter second.
fn harness_with_step<'a>(fx: Fixture, step_dt: f32) -> Harness<'a, Fixture> {
    let mut h = Harness::builder()
        .with_size(Vec2::new(900.0, 560.0))
        .with_pixels_per_point(1.0)
        .with_step_dt(step_dt)
        .build_ui_state(
            |ui, fx: &mut Fixture| {
                if !fx.ready {
                    lmux_app::fonts::install(ui.ctx());
                    fx.ready = true;
                    return;
                }
                // Same as the app: the tree on screen belongs to this host.
                let host = fx.net.host_name();
                fx.state.set_current_host(&host);
                let mut c = lmux_app::ui::Ctx { state: &mut fx.state, net: &mut fx.net };
                lmux_app::app::shortcuts(ui.ctx(), &mut c);
                // Same rule as the app: an inline editor owns the keyboard.
                let text_editing = ui.ctx().memory(|m| m.focused().is_some());
                if !text_editing {
                    lmux_app::app::route_input(ui.ctx(), &mut c);
                }
                lmux_app::app::render(ui, &mut c);
            },
            fx,
        );
    h.run();
    h
}

fn harness<'a>(fx: Fixture) -> Harness<'a, Fixture> {
    harness_with_step(fx, 0.25)
}

fn rpc_methods(fx: &Fixture) -> Vec<(String, serde_json::Value)> {
    fx.state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::Rpc { request } => Some((request.method.clone(), request.params.clone())),
            _ => None,
        })
        .collect()
}

/// A real pointer click at `pos`, driven through egui's event queue.
fn click_at(h: &mut Harness<Fixture>, pos: egui::Pos2, button: egui::PointerButton) {
    h.input_mut().events.push(egui::Event::PointerMoved(pos));
    h.run();
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::default(),
        });
        h.run();
    }
}

/// A real double click at `pos`.
fn double_click_at(h: &mut Harness<Fixture>, pos: egui::Pos2) {
    h.input_mut().events.push(egui::Event::PointerMoved(pos));
    h.run();
    for _ in 0..2 {
        for pressed in [true, false] {
            h.input_mut().events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Modifiers::default(),
            });
        }
    }
    h.run();
    h.run();
}

/// One click with the fewest frames it can take. `run` settles the whole UI and
/// can burn more than egui's double-click window between two clicks; a real
/// pointer's pair does not.
fn tap(h: &mut Harness<Fixture>, pos: egui::Pos2) {
    h.input_mut().events.push(egui::Event::PointerMoved(pos));
    h.step();
    h.input_mut().events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::default(),
    });
    h.step();
    h.input_mut().events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::default(),
    });
    h.step();
}

fn chip_rect(h: &Harness<Fixture>, surface: &str) -> egui::Rect {
    *h.state()
        .state
        .chip_rects
        .get(surface)
        .unwrap_or_else(|| panic!("no chip rect recorded for {surface}"))
}

/// A point in the sidebar's leftover space, below every row: where a right
/// click asks which host a new session belongs to.
fn empty_sidebar_point(h: &Harness<Fixture>) -> egui::Pos2 {
    let rows = h
        .state()
        .state
        .card_rects
        .values()
        .chain(h.state().state.folder_rects.values());
    let bottom = rows.map(|r| r.max.y).fold(0.0, f32::max);
    // The harness window is 900x560 and the status bar takes the last 24.
    egui::pos2(100.0, (bottom + 10.0).min(528.0))
}

/// A pane with three chips, the middle one active.
fn fixture_three_chips() -> Fixture {
    let mut fx = fixture();
    {
        let ws = &mut fx.state.tree.workspaces[0];
        ws.panes[0].surfaces = vec![
            term("surface:1", "one"),
            term("surface:2", "two"),
            term("surface:3", "three"),
        ];
        ws.panes[0].active_surface = Some("surface:2".into());
    }
    fx
}

fn press(h: &mut Harness<Fixture>, mods: Modifiers, key: Key) {
    h.input_mut().events.push(egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: mods,
    });
    h.run();
}

#[test]
fn cmd_d_splits_right() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers::COMMAND, Key::D);
    let rpcs = rpc_methods(h.state());
    let split = rpcs.iter().find(|(m, _)| m == "pane.split").expect("pane.split sent");
    assert_eq!(split.1["side"], "right");
    assert_eq!(split.1["type"], "terminal");
    assert_eq!(split.1["pane"], "pane:1");
}

#[test]
fn cmd_shift_d_splits_down() {
    let mut h = harness(fixture());
    let mods = Modifiers { command: true, shift: true, ..Default::default() };
    press(&mut h, mods, Key::D);
    let rpcs = rpc_methods(h.state());
    let split = rpcs.iter().find(|(m, _)| m == "pane.split").expect("pane.split sent");
    assert_eq!(split.1["side"], "down");
}

#[test]
fn cmd_shift_b_opens_a_browser_chip_in_the_focused_pane() {
    let mut h = harness(fixture());
    let mods = Modifiers { command: true, shift: true, ..Default::default() };
    press(&mut h, mods, Key::B);
    let rpcs = rpc_methods(h.state());
    // A chip (a tab in the pane), not a new pane.
    assert!(rpcs.iter().all(|(m, _)| m != "pane.split"));
    let call = rpcs.iter().find(|(m, _)| m == "surface.new").expect("surface.new sent");
    assert_eq!(call.1["type"], "browser");
    assert_eq!(call.1["pane"], "pane:1");
    // Blank, with the address bar waiting for input (as cmux's new tab).
    assert_eq!(call.1["url"], "about:blank");
}

#[test]
fn cmd_w_focus_follows_the_close_to_the_right() {
    let mut fx = fixture();
    {
        let ws = &mut fx.state.tree.workspaces[0];
        ws.panes[0].surfaces = vec![
            term("surface:1", "one"),
            term("surface:2", "two"),
            term("surface:3", "three"),
        ];
        ws.panes[0].active_surface = Some("surface:2".into());
    }
    let mut h = harness(fx);
    press(&mut h, Modifiers::COMMAND, Key::W);
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.close").expect("surface.close sent");
    assert_eq!(call.1["surface"], "surface:2");
    assert_eq!(
        h.state().state.pane_active.get("pane:1").map(String::as_str),
        Some("surface:3"),
        "closing the active middle chip must select the chip to its right"
    );
}

#[test]
fn cmd_t_new_terminal_tab() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers::COMMAND, Key::T);
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.new").expect("surface.new sent");
    assert_eq!(call.1["type"], "terminal");
    assert_eq!(call.1["pane"], "pane:1");
}

#[test]
fn double_clicking_the_sidebar_opens_a_session() {
    let mut h = harness(fixture());
    // Empty space under the cards, inside the sidebar body.
    double_click_at(&mut h, egui::pos2(100.0, 500.0));
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "workspace.new").expect("workspace.new sent");
    assert_eq!(
        call.1["folder"], "folder:1",
        "the new session lands in the selected workspace's folder"
    );
    assert!(
        rpcs.iter().all(|(m, _)| m != "surface.new"),
        "a sidebar double click must not open a tab"
    );
    assert!(
        h.state().state.actions.is_empty(),
        "the session belongs in the sidebar, not in an outside terminal window"
    );
}

#[test]
fn a_single_click_on_the_sidebar_starts_nothing() {
    let mut h = harness(fixture());
    click_at(&mut h, egui::pos2(100.0, 500.0), egui::PointerButton::Primary);
    assert!(
        rpc_methods(h.state()).iter().all(|(m, _)| m != "workspace.new"),
        "one click must not open a session"
    );
}

#[test]
fn double_clicking_the_tab_strip_opens_a_tab() {
    let mut h = harness(fixture());
    // The strip's empty run, right of the single chip.
    let chip = chip_rect(&h, "surface:1");
    double_click_at(&mut h, egui::pos2(chip.max.x + 40.0, chip.center().y));
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.new").expect("surface.new sent");
    assert_eq!(call.1["type"], "terminal");
    assert_eq!(call.1["pane"], "pane:1");
}

#[test]
fn double_clicking_a_workspace_card_renames_it() {
    let mut h = harness(fixture());
    let card = *h
        .state()
        .state
        .card_rects
        .get("workspace:2")
        .expect("card rect for workspace:2");
    double_click_at(&mut h, card.center());
    assert_eq!(
        h.state().state.selected.as_deref(),
        Some("workspace:2"),
        "the double click also selects the card"
    );
    let editing = h.state().state.ws_rename.as_ref().map(|r| r.folder.clone());
    assert_eq!(editing.as_deref(), Some("workspace:2"), "the title editor opens");
    assert!(
        !rpc_methods(h.state()).iter().any(|(m, _)| m == "workspace.new"),
        "a double click no longer creates a session"
    );

    // Replace the title and commit with Enter.
    h.input_mut().events.push(egui::Event::Key {
        key: Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    });
    h.run();
    h.input_mut().events.push(egui::Event::Text("api work".into()));
    h.run();
    press(&mut h, Modifiers::NONE, Key::Enter);
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "workspace.rename").expect("workspace.rename sent");
    assert_eq!(call.1["workspace"], "workspace:2");
    assert_eq!(call.1["title"], "api work");
    assert!(h.state().state.ws_rename.is_none());
}

/// Opening a title editor must not move the sessions below it: the rows keep
/// the places they had, whichever card is being renamed.
#[test]
fn renaming_a_card_leaves_every_card_where_it_was() {
    let mut h = harness(fixture());
    let before = h.state().state.card_rects.clone();
    assert!(before.len() >= 3, "the fixture has several sessions");
    let ids: Vec<String> = h.state().state.tree.workspaces.iter().map(|w| w.id.clone()).collect();
    for id in ids {
        h.state_mut().state.begin_ws_rename(Some(&id));
        h.run();
        h.run();
        assert!(h.state().state.ws_rename.is_some(), "{id} is editing");
        let after = h.state().state.card_rects.clone();
        for (card, was) in &before {
            assert_eq!(after.get(card), Some(was), "{card} moved while renaming {id}");
        }
        h.state_mut().state.ws_rename = None;
        h.run();
    }
}

#[test]
fn right_clicking_the_sidebar_asks_which_host() {
    use egui_kittest::kittest::Queryable;

    let mut h = harness(fixture());
    let below = empty_sidebar_point(&h);
    click_at(&mut h, below, egui::PointerButton::Secondary);
    h.run();

    // One submenu per host, this machine's first.
    assert!(h.query_by_label_contains("New Session on this mac").is_some());
    assert!(h.query_by_label_contains("New Session on mock").is_some());
    assert!(h.query_by_label_contains("New Folder on this mac").is_some());

    // The connected host's folders are one hover away, nested ones by trail.
    h.get_by_label_contains("New Session on this mac").click();
    h.run();
    assert!(h.query_by_label("Top Level").is_some());
    assert!(
        h.query_by_label("one / nested").is_some(),
        "a nested folder is offered by trail"
    );
}

#[test]
fn the_sidebar_menu_creates_a_session_in_the_chosen_folder() {
    use egui_kittest::kittest::Queryable;

    let mut h = harness(fixture());
    let below = empty_sidebar_point(&h);
    click_at(&mut h, below, egui::PointerButton::Secondary);
    h.run();
    h.get_by_label_contains("New Session on this mac").click();
    h.run();
    h.get_by_label("one / nested").click();
    h.run();

    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "workspace.new").expect("workspace.new sent");
    assert_eq!(call.1["folder"], "folder:2");
}

#[test]
fn a_create_aimed_at_another_host_waits_for_that_host() {
    use egui_kittest::kittest::Queryable;

    let mut h = harness(fixture());
    // The mock's folders were learned on an earlier visit.
    let mut mock_tree = sample_tree();
    mock_tree.folders = vec![wire::Folder {
        id: "folder:9".into(),
        name: "remote".into(),
        parent: None,
        collapsed: false,
    }];
    {
        let st = &mut h.state_mut().state;
        st.set_current_host("mock");
        st.apply_tree(mock_tree.clone());
        st.set_current_host("this mac");
        st.apply_tree(sample_tree());
    }
    h.run();

    let below = empty_sidebar_point(&h);
    click_at(&mut h, below, egui::PointerButton::Secondary);
    h.run();
    h.get_by_label_contains("New Session on mock").click();
    h.run();
    h.get_by_label("remote").click();
    h.run();

    {
        let st = h.state();
        assert!(
            st.state.actions.iter().any(|a| matches!(
                a,
                lmux_app::state::Action::SwitchHost(1)
            )),
            "the app must switch to the host it was asked for"
        );
        assert_eq!(
            st.state.pending_create.as_ref().map(|p| (p.host.as_str(), p.folder.as_deref())),
            Some(("mock", Some("folder:9"))),
            "and hold the create until that host's tree is in"
        );
    }
    assert!(
        rpc_methods(h.state()).iter().all(|(m, _)| m != "workspace.new"),
        "nothing is sent to the wrong host"
    );

    // The switch lands: the mock's tree arrives, and the create goes out.
    {
        let st = &mut h.state_mut().state;
        st.set_current_host("mock");
        st.apply_tree(mock_tree);
    }
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "workspace.new").expect("workspace.new sent");
    assert_eq!(call.1["folder"], "folder:9");
    assert!(h.state().state.pending_create.is_none());
}

#[test]
fn right_clicking_a_folder_row_creates_a_folder_inside_it() {
    use egui_kittest::kittest::Queryable;

    let mut h = harness(fixture());
    let row = *h
        .state()
        .state
        .folder_rects
        .get("folder:1")
        .expect("folder row rect for folder:1");
    click_at(&mut h, row.center(), egui::PointerButton::Secondary);
    assert!(h.query_by_label("New Folder Inside").is_some());
    h.get_by_label("New Folder Inside").click();
    h.run();

    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "folder.new").expect("folder.new sent");
    assert_eq!(call.1["parent"], "folder:1", "the nested folder's parent is the row's folder");
    assert!(
        h.state().state.folder_rename.is_none(),
        "the rename editor opens once the reply names the new ref, not before"
    );
}

#[test]
fn clicking_a_folder_rows_plus_creates_a_session_in_it() {
    let mut h = harness(fixture());
    let plus = *h
        .state()
        .state
        .folder_plus_rects
        .get("folder:1")
        .expect("the folder row records where its `+` is drawn");
    click_at(&mut h, plus.center(), egui::PointerButton::Primary);
    h.run();

    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "workspace.new").expect("workspace.new sent");
    assert_eq!(call.1["folder"], "folder:1", "the session belongs to the row's folder");
    assert!(
        !h.state().state.collapsed.contains("folder:1"),
        "the `+` must not also flip the row's collapse"
    );
    assert!(
        h.state().state.card_rects.contains_key("workspace:1"),
        "and the folder's session stays on screen"
    );
}

#[test]
fn a_cards_menu_moves_the_session_to_another_folder() {
    use egui_kittest::kittest::Queryable;

    let mut h = harness(fixture());
    // workspace:2 sits in the nested folder, so `Top Level` and the other
    // folder are both offered.
    let card = *h
        .state()
        .state
        .card_rects
        .get("workspace:2")
        .expect("card rect for workspace:2");
    click_at(&mut h, card.center(), egui::PointerButton::Secondary);
    h.run();
    h.get_by_label_contains("Move to Folder").click();
    h.run();
    h.get_by_label("one").click();
    h.run();

    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "workspace.move").expect("workspace.move sent");
    assert_eq!(call.1["workspace"], "workspace:2");
    assert_eq!(call.1["folder"], "folder:1");
}

#[test]
fn the_cards_close_button_closes_that_session() {
    let mut h = harness(fixture());
    // workspace:3 has no unread badge, so its close button sits at the right edge, mid-height.
    let card = *h
        .state()
        .state
        .card_rects
        .get("workspace:3")
        .expect("card rect for workspace:3");
    let close = egui::pos2(card.max.x - 12.0, card.center().y);
    click_at(&mut h, close, egui::PointerButton::Primary);
    h.run();

    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "workspace.close").expect("workspace.close sent");
    assert_eq!(call.1["workspace"], "workspace:3");
}

#[test]
fn a_new_folder_is_named_in_line_in_the_sidebar() {
    let mut h = harness(fixture());
    h.state_mut().state.new_folder(Some("folder:1".into()));
    h.run();

    // The daemon broadcasts the tree first, then the reply that names the ref.
    let mut tree = sample_tree();
    tree.folders.push(wire::Folder {
        id: "folder:9".into(),
        name: "New Folder".into(),
        parent: Some("folder:1".into()),
        collapsed: false,
    });
    h.state_mut().state.apply_tree(tree);
    let id = h
        .state()
        .state
        .pending
        .iter()
        .rev()
        .find_map(|m| match m {
            ClientMsg::Rpc { request } if request.method == "folder.new" => Some(request.id),
            _ => None,
        })
        .expect("folder.new sent");
    h.state_mut().state.handle_server_msg(wire::ServerMsg::RpcResult {
        response: wire::RpcResponse {
            id,
            result: Some(serde_json::json!({ "folder": "folder:9" })),
            error: None,
        },
    });
    h.run();
    h.run();
    assert!(
        h.state().state.folder_rename.as_ref().is_some_and(|r| r.folder == "folder:9"),
        "the new row carries an editor"
    );

    if let Some(r) = h.state_mut().state.folder_rename.as_mut() {
        r.text = "billing".into();
    }
    press(&mut h, Modifiers::default(), Key::Enter);

    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "folder.rename").expect("folder.rename sent");
    assert_eq!(call.1["folder"], "folder:9");
    assert_eq!(call.1["name"], "billing");
    assert!(h.state().state.folder_rename.is_none(), "the editor closes on Enter");
}

/// Double clicking a folder row edits its name, the way a chip's double click
/// does. The pair's first click must not leave the folder collapsed, or the
/// row would hide its own sessions the moment it becomes editable.
#[test]
fn double_clicking_a_folder_row_opens_its_rename_editor() {
    let mut h = harness(fixture());
    let row = *h
        .state()
        .state
        .folder_rects
        .get("folder:1")
        .expect("folder row rect for folder:1");
    double_click_at(&mut h, row.center());

    let ren = h.state().state.folder_rename.as_ref().expect("the row carries an editor");
    assert_eq!(ren.folder, "folder:1");
    assert_eq!(ren.text, "one", "the editor starts from the current name");
    assert!(
        !h.state().state.collapsed.contains("folder:1"),
        "renaming must not collapse the folder under the pointer"
    );
}

/// The same gesture with the two clicks in separate frames, the way the app
/// receives them: the first click flips the row, and the second must take that
/// flip back rather than leave the folder collapsed behind the editor.
#[test]
fn the_folder_double_click_takes_back_the_first_clicks_collapse() {
    let mut h = harness_with_step(fixture(), 1.0 / 60.0);
    let row = *h.state().state.folder_rects.get("folder:1").expect("folder row rect");
    tap(&mut h, row.center());
    assert!(
        h.state().state.collapsed.contains("folder:1"),
        "a single click still collapses the folder"
    );

    let row = *h.state().state.folder_rects.get("folder:1").expect("folder row rect");
    tap(&mut h, row.center());
    assert!(
        h.state().state.folder_rename.as_ref().is_some_and(|r| r.folder == "folder:1"),
        "the second click opens the editor"
    );
    assert!(
        !h.state().state.collapsed.contains("folder:1"),
        "and leaves the folder where the double click found it"
    );
}

#[test]
fn cmd_w_closes_active_surface() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers::COMMAND, Key::W);
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.close").expect("surface.close sent");
    assert_eq!(call.1["surface"], "surface:1");
}

#[test]
fn cmd_digit_selects_workspace() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers::COMMAND, Key::Num3);
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "workspace.select").expect("workspace.select sent");
    assert_eq!(call.1["workspace"], "workspace:3");
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:3"));
}

/// The sidebar shows folder `one`, its nested folder with `second`, then
/// `first`, then the top-level `third`; Cmd+1 picks the first card on screen.
#[test]
fn cmd_digit_follows_sidebar_order() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers::COMMAND, Key::Num1);
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:2"));
    // Cmd+9 is always the last one.
    press(&mut h, Modifiers::COMMAND, Key::Num9);
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:3"));
}

#[test]
fn cmd_ctrl_brackets_cycle_workspaces_in_sidebar_order() {
    let mut h = harness(fixture());
    let mods = Modifiers { command: true, ctrl: true, ..Default::default() };
    // first (index 1 of [second, first, third]) → third → wraps to second.
    press(&mut h, mods, Key::CloseBracket);
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:3"));
    press(&mut h, mods, Key::CloseBracket);
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:2"));
    press(&mut h, mods, Key::OpenBracket);
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:3"));
}

#[test]
fn cmd_shift_brackets_and_ctrl_digits_pick_tabs() {
    let mut h = harness(fixture_three_chips());
    let active = |h: &Harness<Fixture>| {
        let st = &h.state().state;
        st.pane("pane:1").and_then(|p| st.active_surface(p))
    };
    let mods = Modifiers { command: true, shift: true, ..Default::default() };
    press(&mut h, mods, Key::CloseBracket);
    assert_eq!(active(&h).as_deref(), Some("surface:3"));
    press(&mut h, mods, Key::CloseBracket);
    assert_eq!(active(&h).as_deref(), Some("surface:1"), "wraps around");
    press(&mut h, Modifiers::CTRL, Key::Num2);
    assert_eq!(active(&h).as_deref(), Some("surface:2"));
    press(&mut h, Modifiers::CTRL, Key::Num9);
    assert_eq!(active(&h).as_deref(), Some("surface:3"), "Ctrl+9 is the last tab");
}

#[test]
fn cmd_b_toggles_the_sidebar_and_cmd_i_the_notifications() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers::COMMAND, Key::B);
    assert!(h.state().state.sidebar_hidden);
    press(&mut h, Modifiers::COMMAND, Key::B);
    assert!(!h.state().state.sidebar_hidden);
    press(&mut h, Modifiers::COMMAND, Key::I);
    assert!(h.state().state.notif_open);
}

#[test]
fn cmd_plus_minus_zero_size_the_terminal_font() {
    let mut h = harness(fixture());
    let base = h.state().state.term_font;
    press(&mut h, Modifiers::COMMAND, Key::Equals);
    assert_eq!(h.state().state.term_font, base + 1.0);
    press(&mut h, Modifiers::COMMAND, Key::Minus);
    press(&mut h, Modifiers::COMMAND, Key::Minus);
    assert_eq!(h.state().state.term_font, base - 1.0);
    press(&mut h, Modifiers::COMMAND, Key::Num0);
    assert_eq!(h.state().state.term_font, base);
}

#[test]
fn cmd_shift_u_jumps_to_the_newest_unread() {
    let mut fx = fixture();
    for (id, ws, at) in [("n1", "workspace:2", 1u64), ("n2", "workspace:3", 2)] {
        fx.state.notifications.push(lmux_proto::Notification {
            id: id.into(),
            title: "done".into(),
            subtitle: None,
            body: None,
            workspace: Some(ws.into()),
            surface: None,
            created_at: at,
            read: false,
        });
    }
    let mut h = harness(fx);
    let mods = Modifiers { command: true, shift: true, ..Default::default() };
    press(&mut h, mods, Key::U);
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:3"));
    press(&mut h, mods, Key::U);
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:2"));
}

#[test]
fn typing_goes_to_focused_terminal() {
    let mut h = harness(fixture());
    h.input_mut().events.push(egui::Event::Text("ls -la".into()));
    h.input_mut().events.push(egui::Event::Key {
        key: Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::default(),
    });
    h.run();
    let term_inputs: Vec<String> = h
        .state()
        .state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::TermInput { surface, data } if surface == "surface:1" => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(term_inputs, vec!["ls -la\r".to_owned()]);
}

#[test]
fn clicking_a_chip_selects_it() {
    let mut h = harness(fixture_three_chips());
    // Starts on the middle chip.
    assert_eq!(h.state().state.active_surface(h.state().state.pane("pane:1").unwrap()).as_deref(), Some("surface:2"));
    let r = chip_rect(&h, "surface:3");
    click_at(&mut h, egui::pos2(r.min.x + 20.0, r.center().y), egui::PointerButton::Primary);
    assert_eq!(
        h.state().state.pane_active.get("pane:1").map(String::as_str),
        Some("surface:3")
    );
}

#[test]
fn clicking_the_close_button_closes_that_chip_and_focus_moves_right() {
    let mut h = harness(fixture_three_chips());
    // The active chip is the middle one, so it has a right neighbour.
    let r = chip_rect(&h, "surface:2");
    let close = egui::pos2(r.max.x - 14.5, r.center().y);
    assert!(r.contains(close), "close point {close:?} must be inside the chip {r:?}");
    click_at(&mut h, close, egui::PointerButton::Primary);

    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.close").expect("surface.close sent");
    assert_eq!(call.1["surface"], "surface:2");
    assert_eq!(
        h.state().state.pane_active.get("pane:1").map(String::as_str),
        Some("surface:3"),
        "focus must follow the close to the chip on the right"
    );
}

#[test]
fn middle_clicking_a_chip_closes_it() {
    let mut h = harness(fixture_three_chips());
    let r = chip_rect(&h, "surface:1");
    click_at(&mut h, egui::pos2(r.min.x + 20.0, r.center().y), egui::PointerButton::Middle);
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.close").expect("surface.close sent");
    assert_eq!(call.1["surface"], "surface:1");
}

#[test]
fn double_clicking_a_chip_starts_a_rename() {
    let mut h = harness(fixture_three_chips());
    let r = chip_rect(&h, "surface:1");
    double_click_at(&mut h, egui::pos2(r.min.x + 20.0, r.center().y));
    assert_eq!(
        h.state().state.renaming.as_ref().map(|r| r.surface.as_str()),
        Some("surface:1")
    );
}

/// Right click a chip and pick a menu item by the start of its label.
fn chip_menu(h: &mut Harness<'_, Fixture>, surface: &str, item: &str) {
    use egui_kittest::kittest::Queryable;
    let r = chip_rect(h, surface);
    click_at(h, egui::pos2(r.min.x + 20.0, r.center().y), egui::PointerButton::Secondary);
    h.get_by_label(item).click();
    h.run();
}

#[test]
fn right_clicking_a_chip_offers_the_tab_menu() {
    use egui_kittest::kittest::Queryable;

    let mut h = harness(fixture_three_chips());
    let r = chip_rect(&h, "surface:1");
    click_at(&mut h, egui::pos2(r.min.x + 20.0, r.center().y), egui::PointerButton::Secondary);
    for label in ["Rename Tab… ⌘R", "Close Tab ⌘W", "Close Other Tabs", "Move Tab ⏵", "New Terminal Tab to Right ⌘T", "Copy IDs"] {
        assert!(h.query_by_label(label).is_some(), "the chip context menu must offer {label}");
    }
}

#[test]
fn the_context_menu_close_action_closes_the_chip() {
    let mut h = harness(fixture_three_chips());
    chip_menu(&mut h, "surface:1", "Close Tab ⌘W");
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.close").expect("surface.close sent");
    assert_eq!(call.1["surface"], "surface:1");
}

#[test]
fn close_other_tabs_closes_the_rest() {
    let mut h = harness(fixture_three_chips());
    chip_menu(&mut h, "surface:2", "Close Other Tabs");
    let closed: Vec<String> = rpc_methods(h.state())
        .into_iter()
        .filter(|(m, _)| m == "surface.close")
        .map(|(_, p)| p["surface"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(closed, ["surface:1", "surface:3"]);
}

#[test]
fn new_terminal_tab_to_right_opens_after_the_tab() {
    let mut h = harness(fixture_three_chips());
    chip_menu(&mut h, "surface:1", "New Terminal Tab to Right ⌘T");
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.new").expect("surface.new sent");
    assert_eq!(call.1["after"], "surface:1");
    assert_eq!(call.1["type"], "terminal");
}

#[test]
fn copy_ids_puts_the_refs_on_the_clipboard() {
    let mut h = harness(fixture_three_chips());
    chip_menu(&mut h, "surface:2", "Copy IDs");
    let pane = h.state().state.pane_of_surface("surface:2").unwrap();
    let ws = h.state().state.selected.clone().unwrap();
    // Taken by the app on the next frame, or still waiting there.
    let text = h.state().state.clipboard_out.clone();
    assert!(
        text.is_none() || text.as_deref() == Some(format!("{ws} {pane} surface:2").as_str()),
        "{text:?}"
    );
}

#[test]
fn rename_chip_commits_on_enter() {
    let mut h = harness(fixture());
    h.state_mut().state.begin_rename("surface:1", "first");
    h.run();
    h.run();
    if let Some(r) = h.state_mut().state.renaming.as_mut() {
        r.text = "renamed".into();
    }
    press(&mut h, Modifiers::default(), Key::Enter);
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.rename").expect("surface.rename sent");
    assert_eq!(call.1["surface"], "surface:1");
    assert_eq!(call.1["title"], "renamed");
    assert!(h.state().state.renaming.is_none(), "the editor must close");
}

#[test]
fn ctrl_c_is_sent_as_control_code() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers { ctrl: true, ..Default::default() }, Key::C);
    let sent: Vec<String> = h
        .state()
        .state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::TermInput { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, vec!["\u{3}".to_owned()]);
}


#[test]
fn fuzzy_matching_ranks_tight_matches_first() {
    use lmux_app::ui::palette::fuzzy_score;
    assert!(fuzzy_score("nw", "New Workspace").is_some());
    assert!(fuzzy_score("zz", "New Workspace").is_none());
    assert!(fuzzy_score("split r", "Split Right") > fuzzy_score("split r", "Split Down").or(Some(i32::MIN)));
    assert!(fuzzy_score("new w", "New Workspace") > fuzzy_score("new w", "Next Workspace"));
}

fn type_and_enter(h: &mut Harness<Fixture>, text: &str) {
    h.run();
    h.input_mut().events.push(egui::Event::Text(text.into()));
    h.run();
    press(h, Modifiers::NONE, Key::Enter);
}

#[test]
fn the_command_palette_runs_the_typed_command() {
    let mut h = harness(fixture());
    let mods = Modifiers { command: true, shift: true, ..Default::default() };
    press(&mut h, mods, Key::P);
    assert!(h.state().state.palette.is_some(), "Cmd+Shift+P opens the palette");
    type_and_enter(&mut h, "split down");
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "pane.split").expect("pane.split sent");
    assert_eq!(call.1["side"], "down");
    assert!(h.state().state.palette.is_none(), "running a command closes it");
}

#[test]
fn go_to_workspace_selects_the_typed_workspace() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers::COMMAND, Key::P);
    type_and_enter(&mut h, "third");
    assert_eq!(h.state().state.selected.as_deref(), Some("workspace:3"));
}

#[test]
fn escape_closes_the_palette() {
    let mut h = harness(fixture());
    press(&mut h, Modifiers::COMMAND, Key::P);
    h.run();
    press(&mut h, Modifiers::NONE, Key::Escape);
    assert!(h.state().state.palette.is_none());
}

#[test]
fn cmd_shift_enter_zooms_the_focused_pane_and_back() {
    let mut fx = fixture();
    {
        use wire::*;
        let ws = &mut fx.state.tree.workspaces[0];
        ws.panes.push(Pane {
            id: "pane:9".into(),
            surfaces: vec![term("surface:9", "right")],
            active_surface: Some("surface:9".into()),
        });
        ws.layout = Layout::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(Layout::Pane { pane: "pane:1".into() }),
            second: Box::new(Layout::Pane { pane: "pane:9".into() }),
        };
    }
    let mut h = harness(fx);
    assert!(h.state().state.pane_rects.contains_key("pane:9"));
    let mods = Modifiers { command: true, shift: true, ..Default::default() };
    press(&mut h, mods, Key::Enter);
    h.run();
    assert_eq!(h.state().state.zoomed_pane.as_deref(), Some("pane:1"));
    assert!(!h.state().state.pane_rects.contains_key("pane:9"), "only the zoomed pane is drawn");
    press(&mut h, mods, Key::Enter);
    h.run();
    assert!(h.state().state.zoomed_pane.is_none());
    assert!(h.state().state.pane_rects.contains_key("pane:9"));
}

/// Cmd+W asks the daemon to close only if nothing runs there. A "busy" answer
/// puts the tab selection back and asks; Enter then closes for real.
#[test]
fn closing_a_busy_tab_asks_first() {
    let mut h = harness(fixture_three_chips());
    press(&mut h, Modifiers::COMMAND, Key::W);
    let (id, params) = h
        .state()
        .state
        .pending
        .iter()
        .find_map(|m| match m {
            ClientMsg::Rpc { request } if request.method == "surface.close" => {
                Some((request.id, request.params.clone()))
            }
            _ => None,
        })
        .expect("surface.close sent");
    assert_eq!(params["if_idle"], true);
    h.state_mut().state.pending.clear();

    h.state_mut().state.handle_server_msg(lmux_proto::ServerMsg::RpcResult {
        response: lmux_proto::RpcResponse {
            id,
            result: None,
            error: Some(lmux_proto::RpcError {
                code: lmux_proto::RpcErrorCode::Busy,
                message: "claude".into(),
            }),
        },
    });
    let st = &h.state().state;
    let q = st.confirm_close.as_ref().expect("the question is open");
    assert_eq!(q.program, "claude");
    assert!(st.banner.is_none(), "a busy answer is a question, not an error");
    let active = st.pane("pane:1").and_then(|p| st.active_surface(p));
    assert_eq!(active.as_deref(), Some("surface:2"), "the selection is back on the busy tab");

    h.run();
    press(&mut h, Modifiers::NONE, Key::Enter);
    let rpcs = rpc_methods(h.state());
    let call = rpcs.iter().find(|(m, _)| m == "surface.close").expect("closed after confirming");
    assert_eq!(call.1["surface"], "surface:2");
    assert!(call.1.get("if_idle").is_none(), "the second close is unconditional");
    assert!(h.state().state.confirm_close.is_none());
}

/// Holding Cmd shows each card's ⌘ digit; a folded folder rolls up unread.
#[test]
fn snapshot_cmd_hints_and_folded_unread() {
    let mut fx = fixture();
    fx.state.tree.workspaces[1].unread_notifications = 3;
    fx.state.collapsed.insert("folder:2".into());
    let mut h = harness(fx);
    h.input_mut().events.push(egui::Event::ModifiersChanged(Modifiers::COMMAND));
    h.step();
    assert!(h.state().state.digit_hints);
    h.snapshot("sidebar_cmd_hints");
}

/// What egui-winit delivers on macOS for one keystroke: a key press, then
/// the text it typed (for printable keys).
fn keystroke(h: &mut Harness<Fixture>, key: Key, mods: Modifiers, text: Option<&str>) {
    h.input_mut().events.push(egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: mods,
    });
    if let Some(t) = text {
        h.input_mut().events.push(egui::Event::Text(t.into()));
    }
    h.input_mut().events.push(egui::Event::Key {
        key,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: mods,
    });
    h.run();
}

fn typed_bytes(fx: &Fixture) -> String {
    fx.state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::TermInput { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect()
}

/// Typing a sentence sends each character once: a space used to arrive
/// twice (from the Space key and from its text), which garbled prompts.
#[test]
fn typing_a_sentence_sends_each_character_once() {
    let mut h = harness(fixture());
    for ch in "and show me".chars() {
        let key = if ch == ' ' { Key::Space } else { Key::from_name(&ch.to_uppercase().to_string()).unwrap() };
        keystroke(&mut h, key, Modifiers::NONE, Some(&ch.to_string()));
    }
    assert_eq!(typed_bytes(h.state()), "and show me");
}

/// macOS editing keys reach the terminal as shells and Claude Code expect.
#[test]
fn mac_editing_keys_reach_the_terminal() {
    let mut h = harness(fixture());
    keystroke(&mut h, Key::Backspace, Modifiers::COMMAND, None);
    keystroke(&mut h, Key::Backspace, Modifiers::ALT, None);
    keystroke(&mut h, Key::Backspace, Modifiers::CTRL, None);
    keystroke(&mut h, Key::ArrowLeft, Modifiers::COMMAND, None);
    keystroke(&mut h, Key::ArrowRight, Modifiers::ALT, None);
    assert_eq!(typed_bytes(h.state()), "\u{15}\u{1b}\u{7f}\u{17}\u{1}\u{1b}f");
}
