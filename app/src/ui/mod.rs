//! Hand-painted UI: everything is drawn with egui's painter to match the
//! density and chrome of cmux, not generic desktop widgets.

pub mod browser;
pub mod drag;
pub mod icons;
pub mod notifications;
pub mod palette;
pub mod pane;
pub mod sidebar;
pub mod tabstrip;
pub mod terminal;
pub mod usage;

use crate::net::Net;
use crate::state::AppState;

pub struct Ctx<'a> {
    pub state: &'a mut AppState,
    pub net: &'a mut Net,
}

/// `"<host> "` on a remote host, empty on this Mac.
pub fn host_prefix(c: &Ctx) -> String {
    if c.net.is_local_host() {
        return String::new();
    }
    c.net
        .current_host()
        .map(|h| format!("{} ", h.ssh_target.clone().unwrap_or_else(|| h.name.clone())))
        .unwrap_or_default()
}

/// The refs of a tab for pasting into a command or a chat. On a remote host
/// the host comes first (`lint-hqc-03 workspace:19 pane:32 surface:43`), since
/// the same refs exist on every host and only mean something with it.
pub fn ids_text(c: &Ctx, pane: Option<&str>, surface: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(host_prefix(c).split_whitespace().map(str::to_owned));
    let ws = c.state.tree.workspaces.iter().find(|w| w.panes.iter().any(|p| p.surfaces.iter().any(|s| s.id == surface)));
    parts.extend(ws.map(|w| w.id.clone()));
    parts.extend(pane.map(str::to_owned));
    parts.push(surface.to_owned());
    parts.join(" ")
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
