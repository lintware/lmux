//! Frame-time instrument for the sidebar while it scrolls.
//!
//! `cargo test --release --test sidebar_cost -- --nocapture` prints the cost of
//! one frame (layout + tessellation) with a long list of full cards.

use std::time::Instant;

use egui::{pos2, vec2, Rect};
use lmux_app::net::{HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::{app, fonts, wire};

const CARDS: usize = 60;

fn fixture() -> (AppState, Net) {
    let workspaces = (0..CARDS)
        .map(|i| wire::Workspace {
            id: format!("workspace:{i}"),
            title: format!("claude · task number {i} with a fairly long title"),
            folder: None,
            cwd: format!("/Users/someone/Documents/projects/client-{i}/services/backend/api"),
            git_branch: Some(format!("feat/some-long-branch-name-{i}")),
            agent: if i % 2 == 0 { wire::AgentState::Running } else { wire::AgentState::Idle },
            summary: Some(
                "Refactored the auth middleware, wired the new token store, and started on the \
                 rate limiter. Next: integration tests and a migration for the sessions table."
                    .into(),
            ),
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: wire::Layout::Pane { pane: format!("pane:{i}") },
            panes: vec![],
            focused_pane: None,
            ports: vec![3000],
            pr: None,
        })
        .collect();
    let tree = wire::Tree {
        host: "h".into(),
        folders: vec![],
        workspaces,
        selected_workspace: Some("workspace:0".into()),
    };
    let mut st = AppState::new();
    st.apply_tree(tree);
    st.selected = Some("workspace:0".into());
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

#[test]
fn sidebar_scroll_frame_cost() {
    let (mut state, mut net) = fixture();
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(2.0);
    fonts::install(&ctx);
    let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0));
    let frame = |ctx: &egui::Context, state: &mut AppState, net: &mut Net, scroll: f32| {
        let mut raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
        raw.events.push(egui::Event::PointerMoved(pos2(100.0, 400.0)));
        if scroll != 0.0 {
            raw.events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0.0, scroll),
                modifiers: Default::default(),
                phase: egui::TouchPhase::Move,
            });
        }
        let out = ctx.run_ui(raw, |ui| {
            let mut c = lmux_app::ui::Ctx { state, net };
            app::render(ui, &mut c);
        });
        ctx.tessellate(out.shapes, out.pixels_per_point)
    };
    for _ in 0..5 {
        frame(&ctx, &mut state, &mut net, 0.0);
    }
    let n = 120;
    let t = Instant::now();
    for i in 0..n {
        frame(&ctx, &mut state, &mut net, if (i / 30) % 2 == 0 { -20.0 } else { 20.0 });
    }
    let per = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("sidebar_cost: {CARDS} cards -> {per:.2} ms/frame while scrolling");
}
