fn main() -> eframe::Result<()> {
    // Launched from the Dock there is no terminal to see a panic in: keep it
    // in ~/.local/state/lmux/app.log, with the time, then let it abort.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = dirs::home_dir().map(|h| h.join(".local/state/lmux")) {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("app.log")) {
                use std::io::Write;
                let when = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let bt = std::backtrace::Backtrace::force_capture();
                let _ = writeln!(f, "[{when}] panic: {info}\n{bt}");
            }
        }
        default_hook(info);
    }));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("lmux")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([420.0, 300.0])
            .with_fullsize_content_view(true)
            .with_title_shown(false)
            // Transparent titlebar: otherwise macOS draws a vibrancy material
            // over the top strip and blurs our crisp glyphs away.
            .with_titlebar_shown(false)
            // An empty icon tells eframe to leave the Dock icon alone, so the
            // bundle's lmux.icns shows; without one eframe swaps in egui's logo.
            .with_icon(egui::IconData::default()),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "lmux",
        options,
        Box::new(|cc| Ok(Box::new(lmux_app::app::LmuxApp::new(cc)))),
    )
}
