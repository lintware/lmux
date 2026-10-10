//! The system clipboard beyond text: reading an image for a paste.

use std::path::PathBuf;

/// When the clipboard holds an image and no text, write it as a PNG to a
/// temporary file and return its path. `None` when it holds text (egui
/// already pasted that) or no image.
#[cfg(target_os = "macos")]
pub fn clipboard_png() -> Option<PathBuf> {
    use objc2_app_kit::{
        NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString,
        NSPasteboardTypeTIFF,
    };
    use objc2_foundation::NSDictionary;

    // Straight from NSPasteboard: `osascript` took about a second for a small
    // screenshot and several for a full screen, most of it spent coercing the
    // image to text just to learn there was none.
    let pb = NSPasteboard::generalPasteboard();
    // SAFETY: the pasteboard type constants are immutable statics.
    let (string_t, png_t, tiff_t) = unsafe { (NSPasteboardTypeString, NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    if pb.stringForType(string_t).is_some_and(|s| s.length() > 0) {
        return None;
    }
    let png = match pb.dataForType(png_t) {
        Some(data) => data,
        None => {
            // Screenshots and most apps also offer TIFF; re-encode it as PNG.
            let tiff = pb.dataForType(tiff_t)?;
            let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
            // SAFETY: an empty properties dictionary is a valid argument.
            unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?
        }
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("lmux-paste-{stamp}.png"));
    std::fs::write(&path, png.to_vec()).ok()?;
    Some(path)
}

#[cfg(not(target_os = "macos"))]
pub fn clipboard_png() -> Option<PathBuf> {
    None
}

/// When Cmd+V last went down, in ms since the epoch (0: not since last taken).
static CMD_V_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Watch for Cmd+V presses straight from AppKit. egui-winit swallows the
/// press when the clipboard has no text, so the only trace egui keeps of an
/// image paste is V's release, and that comes without Cmd whenever Cmd is let
/// go first: the paste was missed about every other time. Call once, on the
/// main thread.
#[cfg(target_os = "macos")]
pub fn watch_cmd_v() {
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};
    use std::ptr::NonNull;

    let block = block2::RcBlock::new(|ev: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit hands the monitor a live event for the call's length.
        let e = unsafe { ev.as_ref() };
        let flags = e.modifierFlags();
        let cmd_only = flags.contains(NSEventModifierFlags::Command)
            && !flags.intersects(NSEventModifierFlags::Shift | NSEventModifierFlags::Option | NSEventModifierFlags::Control);
        // keyCode 9 is V on ANSI; the character covers other layouts.
        let is_v = e.keyCode() == 9 || e.charactersIgnoringModifiers().is_some_and(|s| s.to_string() == "v");
        if cmd_only && is_v {
            CMD_V_AT.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
        }
        ev.as_ptr()
    });
    // SAFETY: called on the main thread; the block returns the event unchanged.
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block) };
    // The monitor lives as long as the app.
    std::mem::forget(monitor);
}

#[cfg(not(target_os = "macos"))]
pub fn watch_cmd_v() {}

/// True once per Cmd+V press seen in the last second.
pub fn take_cmd_v() -> bool {
    let at = CMD_V_AT.swap(0, std::sync::atomic::Ordering::Relaxed);
    at != 0 && now_ms().saturating_sub(at) < 1000
}
