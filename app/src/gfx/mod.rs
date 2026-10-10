//! The terminal renderer: Ghostty's techniques ported to Rust and wgpu.
//! Glyphs are rasterized by CoreText into an atlas, box drawing and blocks
//! are drawn by hand to the exact cell, and the grid is drawn by one set of
//! shaders (`cell.wgsl`) inside egui's frame.

pub mod atlas;
pub mod config;
pub mod constraint;
#[cfg(target_os = "macos")]
pub mod frame;
#[cfg(target_os = "macos")]
pub mod font;
#[cfg(target_os = "macos")]
pub mod glyphs;
pub mod metrics;
#[cfg(target_os = "macos")]
pub mod pipeline;
pub mod sprite;

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Draw terminals with this renderer from now on. The app calls it once its
/// GPU pipelines are installed; tests and `LMUX_RENDERER=egui` keep the
/// egui painter.
pub fn enable() {
    let off = std::env::var("LMUX_RENDERER").is_ok_and(|v| v == "egui");
    ENABLED.store(cfg!(target_os = "macos") && !off, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[cfg(target_os = "macos")]
pub use session::{with_session, Session};

#[cfg(target_os = "macos")]
mod session {
    use std::cell::RefCell;

    use super::config::TermConfig;
    use super::frame::Theme;
    use super::glyphs::{FontConfig, Fonts};

    /// Ghostty's default font size on macOS, in points.
    const DEFAULT_POINTS: f32 = 13.0;

    /// The fonts, atlases and colours every terminal pane draws with.
    pub struct Session {
        pub config: TermConfig,
        pub theme: Theme,
        pub fonts: Fonts,
    }

    thread_local! {
        static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    }

    fn font_config(config: &TermConfig, zoom: f32, scale: f32) -> FontConfig {
        let points = (config.font_size.unwrap_or(DEFAULT_POINTS) + zoom).clamp(4.0, 72.0);
        FontConfig {
            families: [
                config.font_family.clone(),
                config.font_family_bold.clone(),
                config.font_family_italic.clone(),
                config.font_family_bold_italic.clone(),
            ],
            points,
            scale,
            thicken: config.font_thicken,
            adjust_cell_height: None,
        }
    }

    /// Run `f` with the session for this zoom (points added to the base size)
    /// and display scale, rebuilding the fonts when either changed. The user's
    /// Ghostty config is read once, the first time.
    pub fn with_session<R>(zoom: f32, scale: f32, f: impl FnOnce(&mut Session) -> R) -> R {
        SESSION.with(|cell| {
            let mut slot = cell.borrow_mut();
            let session = slot.get_or_insert_with(|| {
                // lmux's window is always dark, so a light:/dark: theme
                // takes its dark half.
                let config = TermConfig::load(true);
                let theme = Theme::from(&config);
                let fonts = Fonts::new(font_config(&config, zoom, scale));
                Session { config, theme, fonts }
            });
            let wanted = font_config(&session.config, zoom, scale);
            if session.fonts.config != wanted {
                session.fonts = Fonts::new(wanted);
            }
            f(session)
        })
    }
}
