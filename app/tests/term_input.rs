//! Terminal input that depends on the program's modes and on the pointer:
//! bracketed paste, selection and copy, mouse reports, URLs, the context menu.
//! Driven through the same `route_input`/`render` path the app uses.

use egui::Vec2;
use egui_kittest::Harness;
use lmux_app::net::{ConnState, HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::wire::{self, ClientMsg};

fn tree() -> wire::Tree {
    use wire::*;
    Tree {
        host: "this mac".into(),
        folders: vec![],
        workspaces: vec![Workspace {
            id: "workspace:1".into(),
            title: "first".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::Idle,
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
                    kind: SurfaceKind::Terminal { tmux_pane: None, cols: 80, rows: 24 },
                }],
                active_surface: Some("surface:1".into()),
            }],
            focused_pane: Some("pane:1".into()),
        }],
        selected_workspace: Some("workspace:1".into()),
    }
}

struct Fixture {
    state: AppState,
    net: Net,
    ready: bool,
}

fn fixture() -> Fixture {
    let mut state = AppState::new();
    state.apply_tree(tree());
    state.selected = Some("workspace:1".into());
    let mut net = Net::with_hosts(vec![HostConfig {
        name: "this mac".into(),
        label: "this mac".into(),
        kind: "direct".into(),
        url: Some("ws://127.0.0.1:7879".into()),
        ssh_target: None,
        remote_port: 7878,
        browser_scale: None,
    }]);
    net.state = ConnState::Connected;
    state.set_current_host("this mac");
    Fixture { state, net, ready: false }
}

/// Frames of 50 ms, well inside the multi-click window.
fn harness<'a>(fx: Fixture) -> Harness<'a, Fixture> {
    let mut h = Harness::builder()
        .with_size(Vec2::new(900.0, 560.0))
        .with_pixels_per_point(1.0)
        .with_step_dt(0.05)
        .build_ui_state(
            |ui, fx: &mut Fixture| {
                if !fx.ready {
                    lmux_app::fonts::install(ui.ctx());
                    fx.ready = true;
                    return;
                }
                let host = fx.net.host_name();
                fx.state.set_current_host(&host);
                let mut c = lmux_app::ui::Ctx { state: &mut fx.state, net: &mut fx.net };
                lmux_app::app::shortcuts(ui.ctx(), &mut c);
                if ui.ctx().memory(|m| m.focused().is_none()) {
                    lmux_app::app::route_input(ui.ctx(), &mut c);
                }
                lmux_app::app::render(ui, &mut c);
            },
            fx,
        );
    h.run();
    h
}

fn term_inputs(fx: &Fixture) -> Vec<String> {
    fx.state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::TermInput { surface, data } if surface == "surface:1" => Some(data.clone()),
            _ => None,
        })
        .collect()
}

fn feed(h: &mut Harness<Fixture>, bytes: &[u8]) {
    h.state_mut().state.ensure_term("surface:1").feed(bytes);
    h.run();
}

#[test]
fn paste_is_bracketed_when_the_program_asks_for_it() {
    let mut h = harness(fixture());
    h.input_mut().events.push(egui::Event::Paste("echo a\necho b".into()));
    h.run();
    assert_eq!(term_inputs(h.state()), vec!["echo a\recho b".to_owned()]);

    h.state_mut().state.pending.clear();
    feed(&mut h, b"\x1b[?2004h");
    h.input_mut().events.push(egui::Event::Paste("rm x\x1b[201~\n".into()));
    h.run();
    assert_eq!(term_inputs(h.state()), vec!["\x1b[200~rm x\n\x1b[201~".to_owned()]);
}
