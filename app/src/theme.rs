//! Colours, metrics and font ids tuned to match `scratchpad/cmux-reference.jpg`.

use egui::{Color32, FontFamily, FontId, Vec2};

// --- colours ---------------------------------------------------------------

/// Sampled from `scratchpad/cmux-reference.jpg`.
pub const SIDEBAR_BG: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x1E);
pub const MAIN_BG: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
pub const HAIRLINE: Color32 = Color32::from_rgb(0x2E, 0x2E, 0x2E);
pub const HAIRLINE_SOFT: Color32 = Color32::from_rgb(0x26, 0x26, 0x26);

/// Top strip, tab strip and status bar sit a touch lighter than the main area.
pub const STRIP_BG: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x1E);
pub const STATUS_BG: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x1E);

pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE6, 0xE6);
pub const TEXT_SOFT: Color32 = Color32::from_rgb(0xC8, 0xC8, 0xCC);
pub const DIM: Color32 = Color32::from_rgb(0x8E, 0x8E, 0x93);
pub const DIM_2: Color32 = Color32::from_rgb(0x6E, 0x6E, 0x74);

pub const ACCENT: Color32 = Color32::from_rgb(0x0A, 0x84, 0xFF);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x38, 0x9B, 0xFF);
pub const GREEN: Color32 = Color32::from_rgb(0x30, 0xD1, 0x58);
pub const RED: Color32 = Color32::from_rgb(0xFF, 0x45, 0x3A);
pub const AMBER: Color32 = Color32::from_rgb(0xFF, 0xB0, 0x2E);

pub const TERM_FG: Color32 = Color32::from_rgb(0xD2, 0xD2, 0xD2);
pub const TERM_BG: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
pub const TERM_CURSOR: Color32 = Color32::from_rgb(0xE0, 0xE0, 0xE0);

pub const CARD_SELECTED: Color32 = Color32::from_rgb(0x0A, 0x84, 0xFF);
pub const BADGE: Color32 = Color32::from_rgb(0x0A, 0x84, 0xFF);

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

// --- fonts -----------------------------------------------------------------

/// Primary proportional UI family (registered by [`crate::fonts`]).
pub const UI_FAMILY: &str = "sfui";
pub const UI_SEMIBOLD_FAMILY: &str = "sfui-sb";
pub const MONO_FAMILY: &str = "sfmono";

pub fn ui(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(UI_FAMILY.into()))
}

pub fn ui_semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(UI_SEMIBOLD_FAMILY.into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MONO_FAMILY.into()))
}

pub const FS_TITLE: f32 = 12.0;
pub const FS_BODY: f32 = 12.0;
pub const FS_SMALL: f32 = 11.0;
pub const FS_TINY: f32 = 10.0;
pub const FS_TERM: f32 = 12.0;

pub fn card_gap() -> Vec2 {
    Vec2::new(0.0, 0.0)
}
