//! Colours, metrics and font ids. A quiet graphite palette: near-neutral
//! ink, soft off-white text, and one muted terracotta accent (the app icon's
//! flame, toned down) kept for the few things that are "current".

use egui::{Color32, FontFamily, FontId, Vec2};

// --- colours ---------------------------------------------------------------

/// Near-neutral ink for the terminal area, a step lighter for chrome.
pub const SIDEBAR_BG: Color32 = Color32::from_rgb(0x18, 0x17, 0x15);
pub const MAIN_BG: Color32 = Color32::from_rgb(0x11, 0x11, 0x10);
pub const HAIRLINE: Color32 = Color32::from_rgb(0x2A, 0x29, 0x26);
pub const HAIRLINE_SOFT: Color32 = Color32::from_rgb(0x22, 0x21, 0x1F);

/// Top strip, tab strip and status bar sit a touch lighter than the main area.
pub const STRIP_BG: Color32 = Color32::from_rgb(0x18, 0x17, 0x15);
pub const STATUS_BG: Color32 = Color32::from_rgb(0x18, 0x17, 0x15);

pub const TEXT: Color32 = Color32::from_rgb(0xE2, 0xDF, 0xD8);
pub const TEXT_SOFT: Color32 = Color32::from_rgb(0xBF, 0xBB, 0xB2);
pub const DIM: Color32 = Color32::from_rgb(0x8C, 0x88, 0x80);
pub const DIM_2: Color32 = Color32::from_rgb(0x64, 0x61, 0x5B);

/// Muted terracotta: the selected card's edge, the active tab, drop targets.
pub const ACCENT: Color32 = Color32::from_rgb(0xC2, 0x6B, 0x48);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0xD4, 0x85, 0x62);
/// A softer, lighter accent for small marks.
pub const FLAME: Color32 = Color32::from_rgb(0xC9, 0x8A, 0x68);
pub const GREEN: Color32 = Color32::from_rgb(0x82, 0xB5, 0x8E);
pub const RED: Color32 = Color32::from_rgb(0xD0, 0x6E, 0x5A);
pub const AMBER: Color32 = Color32::from_rgb(0xD6, 0xAA, 0x68);

pub const TERM_FG: Color32 = Color32::from_rgb(0xDE, 0xDB, 0xD3);
pub const TERM_BG: Color32 = Color32::from_rgb(0x11, 0x11, 0x10);
pub const TERM_CURSOR: Color32 = Color32::from_rgb(0xC9, 0x8A, 0x68);

/// The selected card: a raised neutral plate, not a coloured one.
pub const CARD_SELECTED: Color32 = Color32::from_rgb(0x2B, 0x29, 0x26);
pub const BADGE: Color32 = Color32::from_rgb(0xB0, 0x64, 0x46);
/// Folder icons: present, not shouting.
pub const FOLDER: Color32 = Color32::from_rgb(0x8C, 0x7F, 0x72);

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

/// A faint amber wash behind whatever waits for the user: a card whose agent
/// needs input, and the host header's "waiting" count.
pub fn attention_bg() -> Color32 {
    Color32::from_rgba_unmultiplied(0xD6, 0xAA, 0x68, 16)
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

/// Sidebar text and row size, a multiple of 1.0 (View > Sidebar Text). Window
/// wide, so one atomic holds it; stored as thousandths.
static SIDEBAR_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

pub fn sidebar_scale() -> f32 {
    SIDEBAR_SCALE.load(std::sync::atomic::Ordering::Relaxed) as f32 / 1000.0
}

pub fn set_sidebar_scale(v: f32) {
    let v = v.clamp(0.8, 1.6);
    SIDEBAR_SCALE.store((v * 1000.0).round() as u32, std::sync::atomic::Ordering::Relaxed);
}

/// One step bigger (`+1`), smaller (`-1`), or back to 1.0 (`0`).
pub fn step_sidebar_scale(step: i32) {
    set_sidebar_scale(if step == 0 { 1.0 } else { sidebar_scale() + step as f32 * 0.1 });
}
