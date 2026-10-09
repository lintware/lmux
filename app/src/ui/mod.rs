//! Hand-painted UI: everything is drawn with egui's painter to match the
//! density and chrome of cmux, not generic desktop widgets.

pub mod browser;
pub mod drag;
pub mod icons;
pub mod notifications;
pub mod pane;
pub mod sidebar;
pub mod statusbar;
pub mod tabstrip;
pub mod terminal;

use crate::net::Net;
use crate::state::AppState;

pub struct Ctx<'a> {
    pub state: &'a mut AppState,
    pub net: &'a mut Net,
}

/// Parse `#rrggbb` (as carried by `StatusPill::color`).
pub fn parse_hex(s: &str, fallback: egui::Color32) -> egui::Color32 {
    let s = s.trim_start_matches('#');
    if s.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&s[0..2], 16),
            u8::from_str_radix(&s[2..4], 16),
            u8::from_str_radix(&s[4..6], 16),
        ) {
            return egui::Color32::from_rgb(r, g, b);
        }
    }
    fallback
}

/// Interact with a rect and return a hover-sensitive response.
pub fn click_rect(ui: &mut egui::Ui, rect: egui::Rect, id: egui::Id) -> egui::Response {
    ui.interact(rect, id, egui::Sense::click())
}
