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
