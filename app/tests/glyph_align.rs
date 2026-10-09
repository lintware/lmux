//! Review #4: non-ASCII glyphs (Claude Code's `⏺ ✻`, emoji, CJK) must sit at
//! their exact cell column and advance two cells when double-width, so the rest
//! of the row cannot drift.
//!
//! Renders one terminal row `A ⏺ B 漢 C D` through the real `app::render` and
//! inspects the x positions of the shapes egui was asked to draw.

use egui::{pos2, vec2, Pos2, Rect};
use lmux_app::net::{ConnState, HostConfig, Net};
use lmux_app::state::AppState;
use lmux_app::term::TermView;
use lmux_app::{app, fonts, theme, wire};

fn fixture() -> (AppState, Net) {
    let surface = wire::Surface {
        id: "surface:1".into(),
        title: "align".into(),
        kind: wire::SurfaceKind::Terminal { tmux_pane: None, cols: 100, rows: 30 },
    };
    let pane = wire::Pane {
        id: "pane:1".into(),
        surfaces: vec![surface],
        active_surface: Some("surface:1".into()),
    };
    let ws = wire::Workspace {
        id: "workspace:1".into(),
        title: "glyphs".into(),
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
    let mut t = TermView::new(100, 30);
    // A ⏺(narrow fallback) B 漢(double-width) C D
    t.feed("A\u{23FA}B\u{6F22}CD".as_bytes());
    st.terms.insert("surface:1".into(), t);
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

fn collect_text(shape: &egui::Shape, out: &mut Vec<(Pos2, String)>) {
    match shape {
        egui::Shape::Text(t) => out.push((t.pos, t.galley.text().to_owned())),
        egui::Shape::Vec(v) => {
            for s in v {
                collect_text(s, out);
            }
        }
        _ => {}
    }
}

#[test]
fn non_ascii_glyphs_keep_their_columns() {
    let (mut state, mut net) = fixture();
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.0);
    fonts::install(&ctx);

    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(820.0, 460.0))),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        let mut c = lmux_app::ui::Ctx { state: &mut state, net: &mut net };
        app::render(ui, &mut c);
    });

    let cell_w = ctx.fonts_mut(|f| f.glyph_width(&theme::mono(theme::FS_TERM), 'M'));

    let mut texts = Vec::new();
    for cs in &out.shapes {
        collect_text(&cs.shape, &mut texts);
    }
    out.textures_delta.clear();

    // The fallback glyphs are unique to our row; anchor on them.
    let anchor = texts
        .iter()
        .find(|(_, t)| t == "\u{23FA}")
        .expect("the ⏺ glyph must be drawn as its own shape");
    let (ax, ay) = (anchor.0.x, anchor.0.y);
    let near = |t: &str| -> Option<Pos2> {
        texts
            .iter()
            .find(|(p, s)| s == t && (p.y - ay).abs() < 1.0)
            .map(|(p, _)| *p)
    };

    let a = near("A").expect("run A");
    let b = near("B").expect("run B");
    let cd = near("CD").expect("run CD after the wide glyph");
    let cjk = texts
        .iter()
        .find(|(p, s)| s == "\u{6F22}" && (p.y - ay).abs() < 1.0)
        .map(|(p, _)| *p)
        .expect("the CJK glyph must be its own shape");

    let close = |x: f32, want: f32| (x - want).abs() < 0.5;
    assert!(close(a.x, ax - cell_w), "A at {:?}, want {}", a, ax - cell_w);
    assert!(close(b.x, ax + cell_w), "B at {:?}, want {}", b, ax + cell_w);
    assert!(close(cjk.x, ax + 2.0 * cell_w), "漢 at {:?}, want {}", cjk, ax + 2.0 * cell_w);
    // The wide glyph occupies two cells, so C D start two columns after it.
    assert!(close(cd.x, ax + 4.0 * cell_w), "CD at {:?}, want {}", cd, ax + 4.0 * cell_w);

    out.textures_delta.clear();
    let _ = ConnState::Connected;
}

#[test]
fn fallback_glyphs_are_not_in_the_mono_face() {
    // Evidence that batching a fallback glyph in a run really did drift: these
    // glyphs are not in the mono face at all (advance 0), so a run would let the
    // fallback face's own advance position the rest of the row.
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.0);
    fonts::install(&ctx);
    let raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 200.0))),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |_ui| {});
    out.textures_delta.clear();
    let f = theme::mono(theme::FS_TERM);
    let (cell, ring, cjk) = ctx.fonts_mut(|fo| {
        (
            fo.glyph_width(&f, 'M'),
            fo.glyph_width(&f, '\u{23FA}'),
            fo.glyph_width(&f, '\u{6F22}'),
        )
    });
    println!("cell_width={cell} ring_width={ring} cjk_width={cjk}");
    assert_ne!(ring, cell, "⏺ must not report the mono cell advance");
    assert_ne!(cjk, cell, "漢 must not report the mono cell advance");
}
