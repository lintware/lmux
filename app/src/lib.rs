//! lmux native client (egui/eframe): sidebar, split layout, terminals, browsers.

pub mod app;
pub mod clip;
pub mod cmux_import;
pub mod decode;
pub mod fonts;
pub mod gfx;
pub mod keys;
pub mod layout;
pub mod menu;
pub mod net;
pub mod pty;
pub mod state;
pub mod term;
pub mod termio;
pub mod theme;
pub mod ui;
pub mod util;
pub mod wire;
#[cfg(target_os = "macos")]
pub mod webview;
