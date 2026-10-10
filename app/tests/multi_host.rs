//! Every host in one sidebar: two daemons whose refs collide (`workspace:1` on
//! both) are drawn as two labelled groups, a click on the other host's card
//! selects it there, and only the host on screen streams anything.
//!
//! Run `UPDATE_SNAPSHOTS=1 cargo test --test multi_host` to (re)create the
//! reference image.

use egui::Vec2;
use egui_kittest::Harness;
use lmux_app::net::{ConnState, HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::{app, fonts, wire};

fn host(name: &str, url: &str) -> HostConfig {
    HostConfig {
        name: name.into(),
        label: name.into(),
        kind: "direct".into(),
        url: Some(url.into()),
        ssh_target: None,
        remote_port: 7878,
        browser_scale: None,
        cloud: None,
    }
}

fn hosts() -> Vec<HostConfig> {
    vec![host("this mac", "ws://127.0.0.1:7879"), host("my-server", "ws://127.0.0.1:7900")]
}

/// A tree with one folder and one session in it. Both hosts get the same refs.
fn tree(host: &str, folder: &str, title: &str, cwd: &str) -> wire::Tree {
    use wire::*;
    Tree {
        host: host.into(),
        folders: vec![Folder {
            id: "folder:1".into(),
            name: folder.into(),
            parent: None,
            collapsed: false,
        }],
        workspaces: vec![Workspace {
            id: "workspace:1".into(),
            title: title.into(),
            folder: Some("folder:1".into()),
            cwd: cwd.into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: "pane:1".into() },
            panes: vec![Pane {
                id: "pane:1".into(),
                surfaces: vec![Surface {
                    id: "surface:1".into(),
                    title: "zsh".into(),
                    kind: SurfaceKind::Terminal { tmux_pane: Some("%1".into()), cols: 80, rows: 24 },
                }],
                active_surface: Some("surface:1".into()),
            }],
            focused_pane: Some("pane:1".into()),
            ports: vec![],
            pr: None,
        }],
        selected_workspace: Some("workspace:1".into()),
    }
}

struct Host {
    state: AppState,
    net: Net,
}

struct Fixture {
    hosts: Vec<Host>,
    active: usize,
    ready: bool,
}

fn fixture() -> Fixture {
    let make = |i: usize, t: wire::Tree| {
        let mut net = Net::for_host(hosts(), i);
        net.state = ConnState::Connected;
        let mut state = AppState::new();
        state.host_salt = net.host_name();
        state.set_current_host(&net.host_name());
        state.apply_tree(t);
        // The tree's own selection is not a user action.
        state.activated = false;
        Host { state, net }
    };
    Fixture {
        hosts: vec![
            make(0, tree("this mac", "lmux", "local shell", "/Users/me/Documents/lmux")),
            make(1, tree("my-server", "webapp", "claude · server", "/home/me/webapp")),
        ],
        active: 0,
        ready: false,
    }
}

fn harness<'a>(fx: Fixture, ppp: f32) -> Harness<'a, Fixture> {
    let mut h = Harness::builder()
        .with_size(Vec2::new(620.0, 420.0))
        .with_pixels_per_point(ppp)
        .build_ui_state(
            |ui, fx: &mut Fixture| {
                if !fx.ready {
                    fonts::install(ui.ctx());
                    fx.ready = true;
                    return;
                }
                let mut ctxs: Vec<lmux_app::ui::Ctx> = fx
                    .hosts
                    .iter_mut()
                    .map(|h| lmux_app::ui::Ctx { state: &mut h.state, net: &mut h.net })
                    .collect();
                app::render_hosts(ui, &mut ctxs, fx.active);
            },
            fx,
        );
    h.run();
    h
}

#[test]
fn both_hosts_are_drawn_as_separate_groups() {
    let h = harness(fixture(), 1.0);
    let fx = h.state();
    let local = fx.hosts[0].state.card_rects.get("workspace:1").copied().expect("local card");
    let remote = fx.hosts[1].state.card_rects.get("workspace:1").copied().expect("remote card");
    assert!(remote.min.y > local.max.y, "the remote group sits below the local one");
    // Each host's drop area is its own group, so rows never cross hosts.
    let a = fx.hosts[0].state.sidebar_list_rect.expect("local section");
    let b = fx.hosts[1].state.sidebar_list_rect.expect("remote section");
    assert!(a.max.y <= b.min.y + 0.5, "sections do not overlap: {a:?} {b:?}");
}

/// A title editor sits inside its card, so it must not pull the list's cursor
/// up with it: the next host's header drew over the card while renaming.
#[test]
fn renaming_a_card_keeps_the_next_host_below_it() {
    let mut fx = fixture();
    fx.hosts[0].state.begin_ws_rename(Some("workspace:1"));
    let mut h = harness(fx, 1.0);
    h.run();
    let fx = h.state();
    assert!(fx.hosts[0].state.ws_rename.is_some(), "the card is editing");
    let local = fx.hosts[0].state.card_rects["workspace:1"];
    let remote = fx.hosts[1].state.card_rects["workspace:1"];
    assert!(remote.min.y > local.max.y, "the remote group sits below the edited card: {local:?} {remote:?}");
    let a = fx.hosts[0].state.sidebar_list_rect.expect("local section");
    let b = fx.hosts[1].state.sidebar_list_rect.expect("remote section");
    assert!(a.max.y <= b.min.y + 0.5, "sections do not overlap: {a:?} {b:?}");
    assert!(a.max.y >= local.max.y, "the local section still holds its whole card: {a:?} {local:?}");
}

#[test]
fn renaming_a_folder_keeps_the_next_host_below_it() {
    let mut fx = fixture();
    fx.hosts[0].state.begin_folder_rename("folder:1", "lmux");
    let mut h = harness(fx, 1.0);
    h.run();
    let fx = h.state();
    assert!(fx.hosts[0].state.folder_rename.is_some(), "the folder is editing");
    let local = fx.hosts[0].state.card_rects["workspace:1"];
    let remote = fx.hosts[1].state.card_rects["workspace:1"];
    assert!(remote.min.y > local.max.y, "the remote group sits below the local one: {local:?} {remote:?}");
}

#[test]
fn clicking_the_other_hosts_card_selects_it_there_only() {
    let mut fx = fixture();
    // Nothing selected on the remote yet, so the click is a real change.
    fx.hosts[1].state.selected = None;
    let mut h = harness(fx, 1.0);
    let card = h.state().hosts[1].state.card_rects["workspace:1"];
    h.input_mut().events.push(egui::Event::PointerMoved(card.center()));
    h.run();
    h.input_mut().events.push(egui::Event::PointerButton {
        pos: card.center(),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    h.input_mut().events.push(egui::Event::PointerButton {
        pos: card.center(),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    });
    h.run();
    let fx = h.state();
    assert_eq!(fx.hosts[1].state.selected.as_deref(), Some("workspace:1"));
    assert!(fx.hosts[1].state.activated, "the remote host asks to come on screen");
    assert!(!fx.hosts[0].state.activated, "the local host was not touched");
    let select = fx.hosts[1]
        .state
        .pending
        .iter()
        .any(|m| format!("{m:?}").contains("workspace.select"));
    assert!(select, "the select RPC is queued on the remote host's connection");
    assert!(
        !fx.hosts[0].state.pending.iter().any(|m| format!("{m:?}").contains("workspace.select")),
        "and not on the local one"
    );
}

#[test]
fn only_the_host_on_screen_subscribes() {
    let mut h = harness(fixture(), 1.0);
    h.run();
    let fx = h.state_mut();
    for host in &mut fx.hosts {
        host.state.sync_subscriptions(true);
    }
    // The remote host's terminal is not on screen, so it never subscribes.
    let remote_subs = fx.hosts[1]
        .state
        .pending
        .iter()
        .filter(|m| matches!(m, wire::ClientMsg::Subscribe { .. }))
        .count();
    assert_eq!(remote_subs, 0);
    assert!(!fx.hosts[1].state.pane_rects.contains_key("pane:1"));
    assert!(fx.hosts[0].state.pane_rects.contains_key("pane:1"));
}

#[test]
fn folding_a_host_hides_its_rows() {
    let mut fx = fixture();
    fx.hosts[1].state.host_collapsed = true;
    let h = harness(fx, 1.0);
    let fx = h.state();
    assert!(fx.hosts[0].state.card_rects.contains_key("workspace:1"));
    assert!(fx.hosts[1].state.card_rects.is_empty());
}

#[test]
fn snapshot_sidebar_multi_host() {
    let mut h = harness(fixture(), 2.0);
    h.snapshot("sidebar_multi_host");
}

fn notification(id: &str, title: &str, at: u64) -> lmux_proto::Notification {
    lmux_proto::Notification {
        id: id.into(),
        title: title.into(),
        subtitle: None,
        body: Some("Claude finished and is waiting for you.".into()),
        workspace: Some("workspace:1".into()),
        surface: None,
        created_at: at,
        read: false,
    }
}

/// One panel for every host, newest first, each item naming its host.
#[test]
fn snapshot_notifications_from_every_host() {
    let mut fx = fixture();
    fx.hosts[0].state.notifications.push(notification("n1", "Build finished", 1_000));
    fx.hosts[1].state.notifications.push(notification("n2", "Claude needs input", 2_000));
    fx.hosts[0].state.notif_open = true;
    let mut h = harness(fx, 2.0);
    h.snapshot("notifications_multi_host");
}

/// Typing an SSH target into the Add Remote Host dialog and pressing Enter
/// queues the host to be added, and closes the dialog.
#[test]
fn the_add_host_dialog_queues_the_typed_target() {
    let mut fx = fixture();
    fx.hosts[0].state.add_host_draft = Some(String::new());
    let mut h = harness(fx, 1.0);
    h.run();
    h.input_mut().events.push(egui::Event::Text("cloud-box-1".into()));
    h.run();
    h.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Default::default(),
    });
    h.run();
    let st = &h.state().hosts[0].state;
    assert!(st.add_host_draft.is_none(), "the dialog closed");
    assert!(
        st.actions.iter().any(|a| matches!(a, lmux_app::state::Action::AddHost(t) if t == "cloud-box-1")),
        "AddHost queued: {:?}",
        st.actions
    );
}

/// The New Cloud Box dialog creates nothing until Create, and then queues
/// the box with what was typed.
#[test]
fn the_cloud_dialog_queues_a_box_on_create() {
    use egui_kittest::kittest::Queryable;
    let mut fx = fixture();
    fx.hosts[0].state.cloud_draft = Some(lmux_app::state::CloudDraft::new("aws"));
    let mut h = harness(fx, 1.0);
    h.run();
    h.input_mut().events.push(egui::Event::Text("Dev".into()));
    h.run();
    assert!(h.state().hosts[0].state.actions.is_empty(), "nothing queued while typing");
    h.get_by_label("Create").click();
    h.run();
    assert!(h.state().hosts[0].state.cloud_draft.is_some(), "an uppercase name is refused");
    let d = h.state_mut().hosts[0].state.cloud_draft.as_mut().unwrap();
    d.name = "dev".into();
    h.run();
    h.snapshot("cloud_dialog");
    h.get_by_label("Create").click();
    h.run();
    let st = &h.state().hosts[0].state;
    assert!(st.cloud_draft.is_none(), "the dialog closed");
    assert!(
        st.actions.iter().any(|a| matches!(a, lmux_app::state::Action::CreateCloud(d)
            if d.provider == "aws" && d.name == "dev" && d.size == "t3.medium")),
        "CreateCloud queued: {:?}",
        st.actions
    );
}

/// Cmd+P lists every host's workspaces; picking the remote one selects it on
/// the remote host, which asks to come on screen.
#[test]
fn go_to_workspace_reaches_another_host() {
    let mut fx = fixture();
    fx.hosts[1].state.selected = None;
    fx.hosts[0].state.palette = Some(lmux_app::state::Palette {
        mode: lmux_app::state::PaletteMode::Workspaces,
        query: String::new(),
        index: 0,
    });
    let mut h = harness(fx, 1.0);
    h.run();
    h.input_mut().events.push(egui::Event::Text("server".into()));
    h.run();
    h.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Default::default(),
    });
    h.run();
    let fx = h.state();
    assert_eq!(fx.hosts[1].state.selected.as_deref(), Some("workspace:1"));
    assert!(fx.hosts[1].state.activated);
    assert!(fx.hosts[0].state.palette.is_none());
}

#[test]
fn snapshot_go_to_workspace_palette() {
    let mut fx = fixture();
    fx.hosts[0].state.palette = Some(lmux_app::state::Palette {
        mode: lmux_app::state::PaletteMode::Workspaces,
        query: String::new(),
        index: 1,
    });
    let mut h = harness(fx, 2.0);
    h.snapshot("palette_workspaces");
}

/// A card shows its listening ports and its PR as links under the cwd.
#[test]
fn snapshot_card_ports_and_pr() {
    let mut fx = fixture();
    let ws = &mut fx.hosts[1].state.tree.workspaces[0];
    ws.git_branch = Some("feat/login".into());
    ws.ports = vec![3000, 5173];
    ws.pr = Some(lmux_proto::PullRequest { number: 42, state: "open".into(), url: "https://github.com/x/y/pull/42".into() });
    let mut h = harness(fx, 2.0);
    h.snapshot("card_ports_pr");
}

/// Clicking a port opens it in a browser tab on the workspace's own host.
#[test]
fn opening_a_port_makes_a_browser_tab_in_that_workspace() {
    let mut fx = fixture();
    fx.hosts[1].state.open_in_browser_tab("workspace:1", "http://localhost:3000/");
    let st = &fx.hosts[1].state;
    assert_eq!(st.selected.as_deref(), Some("workspace:1"));
    let call = st
        .pending
        .iter()
        .find_map(|m| match m {
            lmux_app::wire::ClientMsg::Rpc { request } if request.method == "surface.new" => Some(request.params.clone()),
            _ => None,
        })
        .expect("surface.new queued");
    assert_eq!(call["type"], "browser");
    assert_eq!(call["url"], "http://localhost:3000/");
    assert_eq!(call["pane"], "pane:1");
    assert!(fx.hosts[0].state.pending.is_empty(), "nothing goes to the other host");
}
