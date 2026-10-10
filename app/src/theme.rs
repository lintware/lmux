//! Colours, metrics and font ids. The palette is the pixel-fire look of the
//! app icon and https://lmux.lintlabs.org: warm ink, cream text, flame accents.

use egui::{Color32, FontFamily, FontId, Vec2};

// --- colours ---------------------------------------------------------------

/// The site's ink (`#12110e`) for the terminal area, a step lighter for chrome.
pub const SIDEBAR_BG: Color32 = Color32::from_rgb(0x1A, 0x18, 0x14);
pub const MAIN_BG: Color32 = Color32::from_rgb(0x12, 0x11, 0x0E);
pub const HAIRLINE: Color32 = Color32::from_rgb(0x2E, 0x2A, 0x23);
pub const HAIRLINE_SOFT: Color32 = Color32::from_rgb(0x25, 0x22, 0x1C);

/// Top strip, tab strip and status bar sit a touch lighter than the main area.
pub const STRIP_BG: Color32 = Color32::from_rgb(0x1A, 0x18, 0x14);
pub const STATUS_BG: Color32 = Color32::from_rgb(0x1A, 0x18, 0x14);

pub const TEXT: Color32 = Color32::from_rgb(0xF3, 0xF0, 0xE7);
pub const TEXT_SOFT: Color32 = Color32::from_rgb(0xD8, 0xD4, 0xC9);
pub const DIM: Color32 = Color32::from_rgb(0xA3, 0x9E, 0x90);
pub const DIM_2: Color32 = Color32::from_rgb(0x74, 0x72, 0x68);

/// lmux orange-red, the flame's body.
pub const ACCENT: Color32 = Color32::from_rgb(0xD9, 0x43, 0x20);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0xF0, 0x7A, 0x3C);
/// The flame's lighter orange.
pub const FLAME: Color32 = Color32::from_rgb(0xF0, 0x7A, 0x3C);
pub const GREEN: Color32 = Color32::from_rgb(0xB3, 0xBC, 0x88);
pub const RED: Color32 = Color32::from_rgb(0xF0, 0x56, 0x3C);
pub const AMBER: Color32 = Color32::from_rgb(0xFF, 0xC2, 0x7A);

pub const TERM_FG: Color32 = Color32::from_rgb(0xEA, 0xE6, 0xDC);
pub const TERM_BG: Color32 = Color32::from_rgb(0x12, 0x11, 0x0E);
pub const TERM_CURSOR: Color32 = Color32::from_rgb(0xF0, 0x7A, 0x3C);

pub const CARD_SELECTED: Color32 = Color32::from_rgb(0xD9, 0x43, 0x20);
pub const BADGE: Color32 = Color32::from_rgb(0xF0, 0x7A, 0x3C);

// --- metrics ---------------------------------------------------------------

pub const TOP_STRIP_H: f32 = 30.0;

/// Workspace header row at the top of the main area (icon + workspace title).
/// It is deliberately *not* a chip.
/// Height of the top row holding the workspace title.
///
/// This is at least as tall as the macOS titlebar band. Two things depend on
/// it: the row must clear the traffic lights, and the pane tab strips below it
/// must start *outside* that band — when a chip sits inside it, dragging the
/// chip also drags the whole window.
pub const WS_HEADER_H: f32 = 30.0;
/// One tab strip per pane, carrying that pane's surface chips.
pub const PANE_STRIP_H: f32 = 22.0;

pub fn chip_active_bg() -> Color32 {
    Color32::from_white_alpha(12)
}

pub fn chip_hover_bg() -> Color32 {
    Color32::from_white_alpha(7)
}

/// macOS keeps its traffic lights visible over the full-size content view;
/// leave room for them so our own glyphs never sit underneath. Only the real
/// window sets this (kittest snapshots have no OS chrome).
static TRAFFIC_LIGHTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_traffic_lights(on: bool) {
    TRAFFIC_LIGHTS.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn titlebar_inset() -> f32 {
    if TRAFFIC_LIGHTS.load(std::sync::atomic::Ordering::Relaxed) {
        68.0
    } else {
        0.0
    }
}
pub const TAB_STRIP_H: f32 = 32.0;
pub const STATUS_H: f32 = 24.0;
/// Laid over an unfocused split's content (Ghostty's unfocused-split-opacity 0.7).
pub const UNFOCUSED_DIM: Color32 = Color32::from_black_alpha(77);
pub const SIDEBAR_DEFAULT_W: f32 = 216.0;
pub const SIDEBAR_MIN_W: f32 = 168.0;
pub const SIDEBAR_MAX_W: f32 = 380.0;
pub const DIVIDER_W: f32 = 6.0;
pub const SIDEBAR_PAD: f32 = 10.0;
pub const ICON: f32 = 13.0;
/// The sidebar's top strip icons and the header bell, a size up from the tab icons.
pub const STRIP_ICON: f32 = 17.0;

// --- fonts -----------------------------------------------------------------

/// Primary proportional UI family (registered by [`crate::fonts`]).
pub const UI_FAMILY: &str = "sfui";
pub const UI_SEMIBOLD_FAMILY: &str = "sfui-sb";
pub const MONO_FAMILY: &str = "sfmono";
/// Silkscreen, the site's pixel face, for small all-caps labels.
pub const PIXEL_FAMILY: &str = "pixel";

pub fn ui(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(UI_FAMILY.into()))
}

pub fn ui_semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(UI_SEMIBOLD_FAMILY.into()))
}

pub fn pixel(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(PIXEL_FAMILY.into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MONO_FAMILY.into()))
}

pub const FS_TITLE: f32 = 12.0;
pub const FS_BODY: f32 = 12.0;
pub const FS_SMALL: f32 = 11.0;
pub const FS_TINY: f32 = 10.0;
pub const FS_TERM: f32 = 12.0;
/// Folder names in the sidebar, a size up so the tree reads at a glance.
pub const FS_FOLDER: f32 = 13.0;

pub fn card_gap() -> Vec2 {
    Vec2::new(0.0, 0.0)
}
