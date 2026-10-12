//! Paint-cost instrument for the terminal painter (review #13).
//!
//! Renders a synthetic htop-style full-screen colour-bar terminal through the
//! real `app::render` path and counts the shapes egui is asked to draw, so the
//! effect of merging adjacent same-background cells is measurable.
//!
//! `cargo test --test paint_cost -- --nocapture` prints the counts.

use egui::{pos2, vec2, Rect};
use lmux_app::net::{ConnState, HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::term::TermView;
use lmux_app::{app, fonts, wire};

const COLS: u16 = 100;
const ROWS: u16 = 30;

/// Four full-width colour segments per row: 4 merged runs, 400 background cells.
fn bars_term() -> TermView {
    let mut t = TermView::new(COLS, ROWS);
    let seg = COLS / 4;
    let mut s = String::new();
    let colours = [
        (200u8, 40u8, 40u8),
        (40, 200, 40),
        (40, 40, 200),
        (120, 120, 40),
    ];
    for row in 0..ROWS {
        s.push_str(&format!("\x1b[{};1H", row + 1));
        for (r, g, b) in colours {
            s.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
            for _ in 0..seg {
                s.push('X');
            }
        }
    }
    s.push_str("\x1b[0m");
    t.feed(s.as_bytes());
    t
}

fn fixture() -> (AppState, Net) {
    let surface = wire::Surface {
        id: "surface:1".into(),
        title: "bars".into(),
        kind: wire::SurfaceKind::Terminal { tmux_pane: None, cols: COLS, rows: ROWS },
    };
    let pane = wire::Pane {
        id: "pane:1".into(),
        surfaces: vec![surface],
        active_surface: Some("surface:1".into()),
    };
    let ws = wire::Workspace {
        id: "workspace:1".into(),
        title: "w".into(),
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
    st.sidebar_hidden = true;
    st.terms.insert("surface:1".into(), bars_term());
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
    (st, net)
}

fn count(shape: &egui::Shape, rects: &mut usize, texts: &mut usize) {
    match shape {
        egui::Shape::Rect(_) => *rects += 1,
        egui::Shape::Text(_) => *texts += 1,
        egui::Shape::Vec(v) => {
            for s in v {
                count(s, rects, texts);
            }
        }
        _ => {}
    }
}

fn render_and_count() -> (usize, usize) {
    let (mut state, mut net) = fixture();
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.0);
    fonts::install(&ctx);
    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(820.0, 460.0))),
        ..Default::default()
    };
    let out = ctx.run_ui(raw, |ui| {
        let mut c = lmux_app::ui::Ctx { state: &mut state, net: &mut net };
        app::render(ui, &mut c);
    });
    let (mut rects, mut texts) = (0, 0);
    for cs in &out.shapes {
        count(&cs.shape, &mut rects, &mut texts);
    }
    let mut out = out;
    out.textures_delta.clear();
    let _ = ConnState::Connected;
    (rects, texts)
}

#[test]
fn colour_bar_terminal_shape_count() {
    let (rects, texts) = render_and_count();
    println!("paint_cost: COLS={COLS} ROWS={ROWS} -> rect shapes = {rects}, text shapes = {texts}");
    // Per-cell backgrounds cost 4 full-width runs/row (~a few hundred incl.
    // chrome); the old per-cell painter drew rows*cols (~2500) rects.
    assert!(rects < 800, "backgrounds must merge into runs: {rects} rects");
    assert!(texts < 200, "text must stay batched: {texts} shapes");
}
