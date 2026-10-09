//! Review #5: browser wheel events must be sent at the pointer, not the pane
//! centre, so scrolling a panel inside the page hits the element under the
//! cursor. Driven with real egui pointer events against the image rect the UI
//! actually drew (`AppState::browser_image_rects`).

use egui::{Color32, Pos2, Vec2};
use egui_kittest::Harness;
use lmux_app::net::{ConnState, HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::wire::{self, ClientMsg};

fn browser_fixture() -> (AppState, Net) {
    let surface = wire::Surface {
        id: "surface:9".into(),
        title: "app".into(),
        kind: wire::SurfaceKind::Browser {
            url: "https://example.com/".into(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            in_app: false,
        },
    };
    let pane = wire::Pane {
        id: "pane:1".into(),
        surfaces: vec![surface],
        active_surface: Some("surface:9".into()),
    };
    let ws = wire::Workspace {
        id: "workspace:1".into(),
        title: "browse".into(),
        folder: None,
        cwd: "/tmp".into(),
        git_branch: None,
        agent: wire::AgentState::None,
        summary: None,
        unread_notifications: 0,
        status_pills: vec![],
        progress: None,
        layout: wire::Layout::Pane { pane: "pane:1".into() },
        panes: vec![pane],
        focused_pane: Some("pane:1".into()),
        ports: vec![],
        pr: None,
    };
    let tree = wire::Tree {
        host: "h".into(),
        folders: vec![],
        workspaces: vec![ws],
        selected_workspace: Some("workspace:1".into()),
    };
    let mut st = AppState::new();
    st.apply_tree(tree);
    st.selected = Some("workspace:1".into());
    // A decoded 100x100 frame so the image area is drawn and hit-testable.
    let b = st.ensure_browser("surface:9", "https://example.com/");
    b.have_frame = true;
    b.frame_size = (100, 100);
    // Not subscribed: keep the resize debounce out of this test.
    b.pending_image = Some(egui::ColorImage::new([4, 4], vec![Color32::BLACK; 16]));
    let net = Net::with_hosts(vec![HostConfig {
        name: "mock".into(),
        label: "mock".into(),
        kind: "direct".into(),
        url: Some("ws://127.0.0.1:7900".into()),
        ssh_target: None,
        remote_port: 7878,
        browser_scale: None,
        cloud: None,
    }]);
    (st, net)
}

struct Fx {
    state: AppState,
    net: Net,
    ready: bool,
}

#[test]
fn wheel_is_sent_at_the_pointer_not_the_centre() {
    let (state, net) = browser_fixture();
    let fx = Fx { state, net, ready: false };
    let mut h = Harness::builder()
        .with_size(Vec2::new(900.0, 600.0))
        .with_pixels_per_point(1.0)
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

    let image = *h
        .state()
        .state
        .browser_image_rects
        .get("surface:9")
        .expect("browser image rect must be recorded");

    // A point 15% in from the left edge of the image, clearly off-centre.
    let p = Pos2::new(image.min.x + image.width() * 0.15, image.min.y + image.height() * 0.5);
    h.input_mut().events.push(egui::Event::PointerMoved(p));
    h.input_mut().events.push(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: Vec2::new(0.0, 40.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::default(),
    });
    h.run();

    let wheels: Vec<(f64, f64)> = h
        .state()
        .state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::BrowserInput { event: wire::BrowserInputEvent::Wheel { x, y, .. }, .. } => {
                Some((*x, *y))
            }
            _ => None,
        })
        .collect();
    let (x, y) = wheels.last().copied().expect("a wheel event must be sent");

    // CSS px = (pointer - image.min) / scale, scale = image.width / frame width.
    let scale = image.width() / 100.0;
    let want_x = (p.x - image.min.x) as f64 / scale as f64;
    let want_y = (p.y - image.min.y) as f64 / scale as f64;
    assert!((x - want_x).abs() < 1.0, "wheel x={x}, want {want_x} (pointer)");
    assert!((y - want_y).abs() < 1.0, "wheel y={y}, want {want_y}");

    // The old behaviour used the image centre: assert we are clearly not there.
    let centre_x = 50.0;
    assert!((x - centre_x).abs() > 20.0, "wheel must not be at the pane centre (x={x})");

    let _ = ConnState::Connected;
}

// --- cmux-style browser keys, toolbar and links ------------------------------

/// A harness running the app's real input path: shortcuts, then (unless a text
/// field owns the keyboard) route_input, then render.
fn app_harness<'a>(state: AppState, net: Net) -> Harness<'a, Fx> {
    let fx = Fx { state, net, ready: false };
    let mut h = Harness::builder()
        .with_size(Vec2::new(900.0, 600.0))
        .with_pixels_per_point(1.0)
        .build_ui_state(
            |ui, fx: &mut Fx| {
                if !fx.ready {
                    lmux_app::fonts::install(ui.ctx());
                    fx.ready = true;
                    return;
                }
                let mut c = lmux_app::ui::Ctx { state: &mut fx.state, net: &mut fx.net };
                lmux_app::app::shortcuts(ui.ctx(), &mut c);
                let text_editing = ui.ctx().memory(|m| m.focused().is_some());
                if !text_editing {
                    lmux_app::app::route_input(ui.ctx(), &mut c);
                }
                lmux_app::app::render(ui, &mut c);
            },
            fx,
        );
    h.run();
    h.run();
    h
}

fn browser_harness<'a>() -> Harness<'a, Fx> {
    let (state, net) = browser_fixture();
    app_harness(state, net)
}

fn key(h: &mut Harness<Fx>, mods: egui::Modifiers, key: egui::Key) {
    h.input_mut().events.push(egui::Event::ModifiersChanged(mods));
    h.input_mut().events.push(egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: mods,
    });
    h.run();
    h.input_mut().events.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
    h.run();
}

fn rpcs(h: &Harness<Fx>) -> Vec<(u64, String, serde_json::Value)> {
    h.state()
        .state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::Rpc { request } => {
                Some((request.id, request.method.clone(), request.params.clone()))
            }
            _ => None,
        })
        .collect()
}

fn browser_inputs(h: &Harness<Fx>) -> Vec<wire::BrowserInputEvent> {
    h.state()
        .state
        .pending
        .iter()
        .filter_map(|m| match m {
            ClientMsg::BrowserInput { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect()
}

fn reply(h: &mut Harness<Fx>, id: u64, result: serde_json::Value) {
    h.state_mut().state.handle_server_msg(wire::ServerMsg::RpcResult {
        response: wire::RpcResponse { id, result: Some(result), error: None },
    });
}

/// Centre of the toolbar's third button (reload / stop).
fn reload_button(h: &Harness<Fx>) -> Pos2 {
    let pane = h.state().state.pane_rects["pane:1"];
    let top = pane.min.y + lmux_app::theme::PANE_STRIP_H;
    Pos2::new(pane.min.x + 8.0 + 2.0 * 26.0 + 10.0, top + 14.0)
}

fn address_bar(h: &Harness<Fx>) -> Pos2 {
    let pane = h.state().state.pane_rects["pane:1"];
    let top = pane.min.y + lmux_app::theme::PANE_STRIP_H;
    Pos2::new(pane.center().x, top + 14.0)
}

fn click(h: &mut Harness<Fx>, p: Pos2, button: egui::PointerButton, mods: egui::Modifiers) {
    h.input_mut().events.push(egui::Event::PointerMoved(p));
    h.run();
    h.input_mut().events.push(egui::Event::ModifiersChanged(mods));
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton { pos: p, button, pressed, modifiers: mods });
        h.run();
    }
    h.input_mut().events.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
    h.run();
}

#[test]
fn cmd_brackets_r_and_zoom_act_on_a_focused_browser() {
    let mut h = browser_harness();
    let font = h.state().state.term_font;
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::OpenBracket);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::CloseBracket);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::R);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::Equals);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::Minus);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::Num0);
    let calls: Vec<(String, serde_json::Value)> =
        rpcs(&h).into_iter().map(|(_, m, p)| (m, p)).collect();
    let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        ["browser.back", "browser.forward", "browser.reload", "browser.zoom", "browser.zoom", "browser.zoom"]
    );
    assert!(calls.iter().all(|(_, p)| p["surface"] == "surface:9"));
    let steps: Vec<i64> = calls[3..].iter().map(|(_, p)| p["step"].as_i64().unwrap()).collect();
    assert_eq!(steps, [1, -1, 0]);
    // Not the terminal's font, not a tab rename, and nothing reached the page.
    assert_eq!(h.state().state.term_font, font);
    assert!(h.state().state.renaming.is_none());
    assert!(browser_inputs(&h).is_empty());
}

#[test]
fn cmd_l_focuses_the_address_bar_and_enter_navigates() {
    let mut h = browser_harness();
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::L);
    assert_eq!(
        h.state().state.browsers["surface:9"].url_draft.as_deref(),
        Some("https://example.com/")
    );
    assert!(h.ctx.memory(|m| m.focused().is_some()), "the address bar must own the keyboard");
    // The whole address is selected, so typing replaces it.
    h.input_mut().events.push(egui::Event::Text("localhost:3000".into()));
    h.run();
    key(&mut h, egui::Modifiers::NONE, egui::Key::Enter);
    let goto = rpcs(&h).into_iter().find(|(_, m, _)| m == "browser.goto").expect("browser.goto sent");
    assert_eq!(goto.2["url"], "http://localhost:3000");
    assert!(h.state().state.browsers["surface:9"].url_draft.is_none());
    // Typing went to the address bar, not the page.
    assert!(browser_inputs(&h).is_empty());
}

#[test]
fn words_in_the_address_bar_search_and_escape_restores() {
    let mut h = browser_harness();
    let bar = address_bar(&h);
    click(&mut h, bar, egui::PointerButton::Primary, egui::Modifiers::NONE);
    h.input_mut().events.push(egui::Event::Text("rust egui".into()));
    h.run();
    key(&mut h, egui::Modifiers::NONE, egui::Key::Escape);
    assert!(h.state().state.browsers["surface:9"].url_draft.is_none(), "Escape ends the edit");
    assert!(rpcs(&h).iter().all(|(_, m, _)| m != "browser.goto"));
    assert_eq!(h.state().state.browsers["surface:9"].url, "https://example.com/");

    click(&mut h, bar, egui::PointerButton::Primary, egui::Modifiers::NONE);
    h.input_mut().events.push(egui::Event::Text("rust egui".into()));
    h.run();
    key(&mut h, egui::Modifiers::NONE, egui::Key::Enter);
    let goto = rpcs(&h).into_iter().find(|(_, m, _)| m == "browser.goto").expect("browser.goto sent");
    assert_eq!(goto.2["url"], "https://www.google.com/search?q=rust+egui");
}

#[test]
fn reload_becomes_stop_while_loading() {
    let mut h = browser_harness();
    let button = reload_button(&h);
    click(&mut h, button, egui::PointerButton::Primary, egui::Modifiers::NONE);
    h.state_mut().state.browsers.get_mut("surface:9").unwrap().loading = true;
    h.run();
    click(&mut h, button, egui::PointerButton::Primary, egui::Modifiers::NONE);
    let methods: Vec<String> = rpcs(&h).into_iter().map(|(_, m, _)| m).collect();
    assert_eq!(methods, ["browser.reload", "browser.stop"]);
}

#[test]
fn cmd_chords_reach_the_page_with_meta_and_paste_types_local_text() {
    let mut h = browser_harness();
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::A);
    key(&mut h, egui::Modifiers::COMMAND, egui::Key::Z);
    h.input_mut().events.push(egui::Event::Paste("from the mac".into()));
    h.run();
    let inputs = browser_inputs(&h);
    let keys: Vec<(String, bool)> = inputs
        .iter()
        .filter_map(|e| match e {
            wire::BrowserInputEvent::Key { key, modifiers, .. } => Some((key.clone(), modifiers.meta)),
            _ => None,
        })
        .collect();
    assert_eq!(keys, [("a".to_string(), true), ("z".to_string(), true)]);
    assert!(inputs.iter().any(|e| matches!(
        e,
        wire::BrowserInputEvent::InsertText { text } if text == "from the mac"
    )));
}

#[test]
fn cmd_c_copies_the_pages_selection_to_this_macs_clipboard() {
    let mut h = browser_harness();
    h.input_mut().events.push(egui::Event::Copy);
    h.run();
    let (id, _, params) = rpcs(&h)
        .into_iter()
        .find(|(_, m, _)| m == "browser.eval")
        .expect("copy asks the page for its selection");
    assert_eq!(params["surface"], "surface:9");
    assert!(params["script"].as_str().unwrap().contains("getSelection"));
    reply(&mut h, id, serde_json::json!({ "value": "selected words" }));
    h.step();
    let copied = h.output().platform_output.commands.iter().any(|c| {
        matches!(c, egui::OutputCommand::CopyText(t) if t == "selected words")
    });
    assert!(copied, "the selection must land on the local clipboard");
}

#[test]
fn middle_and_cmd_click_open_the_link_in_a_new_tab() {
    for (button, mods) in [
        (egui::PointerButton::Middle, egui::Modifiers::NONE),
        (egui::PointerButton::Primary, egui::Modifiers::COMMAND),
    ] {
        let mut h = browser_harness();
        let image = h.state().state.browser_image_rects["surface:9"];
        click(&mut h, image.center(), button, mods);
        let (id, _, params) = rpcs(&h)
            .into_iter()
            .find(|(_, m, _)| m == "browser.link_at")
            .expect("asks for the link under the pointer");
        assert!((params["x"].as_f64().unwrap() - 50.0).abs() < 1.0);
        // The page itself never gets that click.
        assert!(
            browser_inputs(&h).iter().all(|e| !matches!(e, wire::BrowserInputEvent::Mouse { .. })),
            "{button:?}: no click reaches the page"
        );
        reply(&mut h, id, serde_json::json!({ "href": "https://example.com/next" }));
        let open = rpcs(&h)
            .into_iter()
            .find(|(_, m, _)| m == "surface.new")
            .expect("a new tab for the link");
        assert_eq!(open.2["pane"], "pane:1");
        assert_eq!(open.2["type"], "browser");
        assert_eq!(open.2["url"], "https://example.com/next");
    }
}

#[test]
fn a_click_on_no_link_opens_nothing() {
    let mut h = browser_harness();
    let image = h.state().state.browser_image_rects["surface:9"];
    click(&mut h, image.center(), egui::PointerButton::Middle, egui::Modifiers::NONE);
    let (id, _, _) = rpcs(&h).into_iter().find(|(_, m, _)| m == "browser.link_at").unwrap();
    reply(&mut h, id, serde_json::json!({ "href": null }));
    assert!(rpcs(&h).iter().all(|(_, m, _)| m != "surface.new"));
}

#[test]
fn a_new_browser_tab_opens_blank_with_the_address_bar_focused() {
    let mut h = browser_harness();
    let mods = egui::Modifiers { command: true, shift: true, ..Default::default() };
    key(&mut h, mods, egui::Key::B);
    let (id, _, params) = rpcs(&h).into_iter().find(|(_, m, _)| m == "surface.new").unwrap();
    assert_eq!(params["url"], "about:blank");
    reply(&mut h, id, serde_json::json!({ "surface": "surface:9" }));
    assert!(h.state().state.browsers["surface:9"].focus_url);
    h.run();
    assert!(h.ctx.memory(|m| m.focused().is_some()), "the address bar takes the keyboard");
}
