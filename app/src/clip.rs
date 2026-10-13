//! The system clipboard beyond text: reading an image for a paste.

use std::path::PathBuf;

/// How the app reads the system clipboard for a paste. Tests replace the
/// readers so they never touch the developer's real pasteboard.
#[derive(Clone, Copy)]
pub struct ClipboardReaders {
    pub files: fn() -> Vec<PathBuf>,
    pub png: fn() -> Option<PathBuf>,
}

impl Default for ClipboardReaders {
    fn default() -> Self {
        Self { files: clipboard_files, png: clipboard_png }
    }
}

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

/// Files copied in Finder (Cmd+C on a file): their paths. Empty when the
/// clipboard holds no file. Finder also puts the file's name on as text,
/// which is all egui would paste.
#[cfg(target_os = "macos")]
pub fn clipboard_files() -> Vec<PathBuf> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
    let pb = NSPasteboard::generalPasteboard();
    // SAFETY: the pasteboard type constant is an immutable static.
    let url_t = unsafe { NSPasteboardTypeFileURL };
    let Some(items) = pb.pasteboardItems() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| item.stringForType(url_t))
        .filter_map(|url| file_url_path(&url.to_string()))
        .collect()
}

#[cfg(not(target_os = "macos"))]
pub fn clipboard_files() -> Vec<PathBuf> {
    Vec::new()
}

/// `file:///Users/a/My%20File.pdf` → `/Users/a/My File.pdf`.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let path = String::from_utf8(out).ok()?;
    let path = path.trim_end_matches('/');
    (!path.is_empty()).then(|| PathBuf::from(path))
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
        let ch = e.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let cmd_shift = flags.contains(NSEventModifierFlags::Command)
            && !flags.intersects(NSEventModifierFlags::Option | NSEventModifierFlags::Control);
        if cmd_shift && is_edit_chord(&ch, shift) {
            // The Edit menu's key equivalents take these before the window
            // sees them, and with no page to answer `paste:` the key is lost.
            // Give it to the winit view directly so egui sees the chord. Any
            // other responder (a page, a native file picker's text field)
            // keeps the menu's route.
            let responder = e.window(objc2::MainThreadMarker::new().expect("main thread")).and_then(|w| w.firstResponder());
            if let Some(r) = responder {
                let class = r.class().name().to_string_lossy().into_owned();
                if is_winit_view(&class) {
                    r.keyDown(e);
                    return std::ptr::null_mut();
                }
            }
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

/// The Edit menu's Cmd chords: undo (Z), redo (Shift+Z), cut, copy, paste and
/// select all (X, C, V, A without Shift). `ch` is the key ignoring modifiers.
fn is_edit_chord(ch: &str, shift: bool) -> bool {
    // With Shift held AppKit's "ignoring modifiers" characters are uppercase.
    match ch.to_lowercase().as_str() {
        "z" => true,
        "x" | "c" | "v" | "a" => !shift,
        _ => false,
    }
}

/// Whether an AppKit class name is winit's content view, the only responder
/// the bypass feeds: pages and native text fields answer the menu themselves.
fn is_winit_view(class: &str) -> bool {
    class == "WinitView"
}

/// True once per Cmd+V press seen in the last second.
pub fn take_cmd_v() -> bool {
    let at = CMD_V_AT.swap(0, std::sync::atomic::Ordering::Relaxed);
    at != 0 && now_ms().saturating_sub(at) < 1000
}

#[cfg(test)]
mod tests {
    #[test]
    fn edit_chords_skip_the_menu_unless_a_page_is_first_responder() {
        for c in ["x", "c", "v", "a"] {
            assert!(is_edit_chord(c, false));
            assert!(!is_edit_chord(c, true));
        }
        // Undo and redo are in the Edit menu too.
        assert!(is_edit_chord("z", false));
        assert!(is_edit_chord("z", true));
        assert!(is_edit_chord("Z", true));
        assert!(!is_edit_chord("V", true));
        assert!(!is_edit_chord("r", false));
        assert!(is_winit_view("WinitView"));
        assert!(!is_winit_view("NSKVONotifying_wry::wkwebview::class::wry_web_view::WryWebView0.57.0"));
        assert!(!is_winit_view("WKWebView"));
        assert!(!is_winit_view("NSTextView"));
    }

    use super::{file_url_path, is_edit_chord, is_winit_view};
    use std::path::PathBuf;

    #[test]
    fn file_urls_decode_to_paths() {
        assert_eq!(file_url_path("file:///Users/a/My%20File.pdf"), Some(PathBuf::from("/Users/a/My File.pdf")));
        assert_eq!(file_url_path("file://localhost/tmp/x/"), Some(PathBuf::from("/tmp/x")));
        assert_eq!(file_url_path("https://example.com"), None);
    }
}
