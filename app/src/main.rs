fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("lmux")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([420.0, 300.0])
            .with_fullsize_content_view(true)
            .with_title_shown(false)
            // Transparent titlebar: otherwise macOS draws a vibrancy material
            // over the top strip and blurs our crisp glyphs away.
            .with_titlebar_shown(false),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "lmux",
        options,
        Box::new(|cc| Ok(Box::new(lmux_app::app::LmuxApp::new(cc)))),
    )
}
