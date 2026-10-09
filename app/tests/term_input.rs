//! Terminal input that depends on the program's modes and on the pointer:
//! bracketed paste, selection and copy, mouse reports, URLs, the context menu.
//! Driven through the same `route_input`/`render` path the app uses.

use egui::{Modifiers, PointerButton, Pos2, Vec2};
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
    h.step();
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

/// Screen position of the centre of viewport cell `(row, col)`.
fn cell(h: &Harness<Fixture>, row: usize, col: usize) -> Pos2 {
    let tv = &h.state().state.terms["surface:1"];
    tv.grid_origin + Vec2::new((col as f32 + 0.5) * tv.cell_size.x, (row as f32 + 0.5) * tv.cell_size.y)
}

/// The left quarter of a cell: a selection that starts here includes it.
fn cell_left(h: &Harness<Fixture>, row: usize, col: usize) -> Pos2 {
    let w = h.state().state.terms["surface:1"].cell_size.x;
    cell(h, row, col) - Vec2::new(w * 0.25, 0.0)
}

fn button(h:&mut Harness<Fixture>, pos: Pos2, b: PointerButton, pressed: bool, modifiers: Modifiers) {
    h.input_mut().events.push(egui::Event::PointerButton { pos, button: b, pressed, modifiers });
    h.step();
}

fn move_to(h: &mut Harness<Fixture>, pos: Pos2) {
    h.input_mut().events.push(egui::Event::PointerMoved(pos));
    h.step();
}

fn click(h: &mut Harness<Fixture>, pos: Pos2, b: PointerButton, modifiers: Modifiers) {
    move_to(h, pos);
    button(h, pos, b, true, modifiers);
    button(h, pos, b, false, modifiers);
}

fn drag(h: &mut Harness<Fixture>, from: Pos2, to: Pos2, modifiers: Modifiers) {
    move_to(h, from);
    button(h, from, PointerButton::Primary, true, modifiers);
    move_to(h, (from + to.to_vec2()) / 2.0);
    move_to(h, to);
    button(h, to, PointerButton::Primary, false, modifiers);
}

fn selection(h: &Harness<Fixture>) -> Option<String> {
    h.state().state.terms["surface:1"].selection_text()
}

/// Copy with Cmd+C (egui-winit delivers it as `Event::Copy`) and return what
/// reached the clipboard this frame.
fn copy(h: &mut Harness<Fixture>) -> Option<String> {
    h.input_mut().events.push(egui::Event::Copy);
    h.step();
    h.output().platform_output.commands.iter().find_map(|c| match c {
        egui::OutputCommand::CopyText(t) => Some(t.clone()),
        _ => None,
    })
}

fn none() -> Modifiers {
    Modifiers::default()
}

#[test]
fn dragging_selects_text_and_cmd_c_copies_it() {
    let mut h = harness(fixture());
    feed(&mut h, b"hello world\r\nsecond line");
    let (from, to) = (cell_left(&h, 0, 0), cell(&h, 1, 5));
    drag(&mut h, from, to, none());
    assert_eq!(selection(&h).as_deref(), Some("hello world\nsecond"));
    assert_eq!(copy(&mut h).as_deref(), Some("hello world\nsecond"));
    assert!(term_inputs(h.state()).is_empty(), "selecting must not type anything");
}

#[test]
fn double_click_selects_a_word_and_triple_click_a_line() {
    let mut h = harness(fixture());
    feed(&mut h, b"hello world\r\nsecond line");
    let p = cell(&h, 0, 8);
    click(&mut h, p, PointerButton::Primary, none());
    click(&mut h, p, PointerButton::Primary, none());
    assert_eq!(selection(&h).as_deref(), Some("world"));
    click(&mut h, p, PointerButton::Primary, none());
    assert_eq!(selection(&h).as_deref().map(str::trim_end), Some("hello world"));
}

#[test]
fn a_plain_click_clears_the_selection() {
    let mut h = harness(fixture());
    feed(&mut h, b"hello world");
    let (from, to) = (cell_left(&h, 0, 0), cell(&h, 0, 4));
    drag(&mut h, from, to, none());
    assert_eq!(selection(&h).as_deref(), Some("hello"));
    // Far enough in time that it is not a double click.
    for _ in 0..10 {
        h.step();
    }
    let p = cell(&h, 0, 2);
    click(&mut h, p, PointerButton::Primary, none());
    assert_eq!(selection(&h), None);
    assert_eq!(copy(&mut h), None, "nothing selected copies nothing");
}

#[test]
fn cmd_click_opens_the_url_under_the_pointer() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let opened = Rc::new(RefCell::new(Vec::<String>::new()));
    let sink = opened.clone();
    lmux_app::termio::set_url_opener(Some(Box::new(move |u: &str| sink.borrow_mut().push(u.to_owned()))));

    let mut h = harness(fixture());
    // Even with mouse reporting on, Cmd+click is ours.
    feed(&mut h, b"docs: https://example.com/a.\r\n\x1b]8;;https://b.dev/\x1b\\link\x1b]8;;\x1b\\\x1b[?1000h");
    let cmd = Modifiers { command: true, mac_cmd: true, ..Default::default() };

    let p = cell(&h, 0, 10);
    click(&mut h, p, PointerButton::Primary, none());
    assert!(opened.borrow().is_empty(), "a click without Cmd opens nothing");

    h.input_mut().events.push(egui::Event::ModifiersChanged(cmd));
    let p = cell(&h, 0, 10);
    click(&mut h, p, PointerButton::Primary, cmd);
    let p = cell(&h, 1, 2);
    click(&mut h, p, PointerButton::Primary, cmd);
    let p = cell(&h, 0, 2);
    click(&mut h, p, PointerButton::Primary, cmd);
    lmux_app::termio::set_url_opener(None);

    assert_eq!(*opened.borrow(), vec!["https://example.com/a".to_owned(), "https://b.dev/".to_owned()]);
    let reports = term_inputs(h.state());
    assert_eq!(reports.len(), 4, "only the two clicks off a link reach the program: {reports:?}");
}

fn wheel(h:&mut Harness<Fixture>, pos: Pos2, lines: f32) {
    move_to(h, pos);
    h.input_mut().events.push(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: Vec2::new(0.0, lines),
        modifiers: Modifiers::default(),
        phase: egui::TouchPhase::Move,
    });
    h.step();
}

#[test]
fn clicks_are_reported_to_a_program_that_asks_for_them() {
    let mut h = harness(fixture());
    // Click reporting, SGR encoding.
    feed(&mut h, b"hello\x1b[?1000h\x1b[?1006h");
    let p = cell(&h, 2, 4);
    click(&mut h, p, PointerButton::Primary, none());
    assert_eq!(term_inputs(h.state()), vec!["\x1b[<0;5;3M".to_owned(), "\x1b[<0;5;3m".to_owned()]);
    assert_eq!(selection(&h), None);

    h.state_mut().state.pending.clear();
    click(&mut h, p, PointerButton::Secondary, none());
    assert_eq!(term_inputs(h.state()), vec!["\x1b[<2;5;3M".to_owned(), "\x1b[<2;5;3m".to_owned()]);
}

#[test]
fn x10_is_the_fallback_encoding() {
    let mut h = harness(fixture());
    feed(&mut h, b"\x1b[?1000h");
    let p = cell(&h, 0, 0);
    click(&mut h, p, PointerButton::Primary, none());
    assert_eq!(term_inputs(h.state()), vec!["\x1b[M !!".to_owned(), "\x1b[M#!!".to_owned()]);
}

#[test]
fn drags_are_reported_once_per_cell() {
    let mut h = harness(fixture());
    feed(&mut h, b"\x1b[?1002h\x1b[?1006h");
    let (from, to) = (cell(&h, 0, 0), cell(&h, 0, 3));
    drag(&mut h, from, to, none());
    let sent = term_inputs(h.state()).concat();
    assert_eq!(sent, "\x1b[<0;1;1M\x1b[<32;3;1M\x1b[<32;4;1M\x1b[<0;4;1m");
}

#[test]
fn shift_forces_a_local_selection_while_the_program_reports() {
    let mut h = harness(fixture());
    feed(&mut h, b"hello world\x1b[?1000h\x1b[?1006h");
    let shift = Modifiers { shift: true, ..Default::default() };
    let (from, to) = (cell_left(&h, 0, 0), cell(&h, 0, 4));
    drag(&mut h, from, to, shift);
    assert_eq!(selection(&h).as_deref(), Some("hello"));
    assert!(term_inputs(h.state()).is_empty(), "a shift-drag must not reach the program");
}

#[test]
fn the_wheel_is_reported_when_the_program_asks_and_scrolls_otherwise() {
    let mut h = harness(fixture());
    for i in 0..60 {
        feed(&mut h, format!("line {i}\r\n").as_bytes());
    }
    let p = cell(&h, 1, 1);
    wheel(&mut h, p, 3.0);
    assert_eq!(h.state().state.terms["surface:1"].display_offset(), 3, "no reporting: scroll back locally");
    assert!(term_inputs(h.state()).is_empty());

    wheel(&mut h, p, -3.0);
    feed(&mut h, b"\x1b[?1000h\x1b[?1006h");
    wheel(&mut h, p, 2.0);
    assert_eq!(term_inputs(h.state()).concat(), "\x1b[<64;2;2M\x1b[<64;2;2M");
    h.state_mut().state.pending.clear();
    wheel(&mut h, p, -1.0);
    assert_eq!(term_inputs(h.state()).concat(), "\x1b[<65;2;2M");
    assert_eq!(h.state().state.terms["surface:1"].display_offset(), 0);
}
