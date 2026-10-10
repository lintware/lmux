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
