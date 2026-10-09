//! The system clipboard beyond text: reading an image for a paste.

use std::path::PathBuf;

/// When the clipboard holds an image and no text, write it as a PNG to a
/// temporary file and return its path. `None` when it holds text (egui
/// already pasted that) or no image.
#[cfg(target_os = "macos")]
pub fn clipboard_png() -> Option<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("lmux-paste-{stamp}.png"));
    let script = format!(
        r#"try
    set t to (the clipboard as text)
    if t is not "" then return "text"
end try
try
    set d to (the clipboard as «class PNGf»)
on error
    return "none"
end try
set f to open for access (POSIX file "{}") with write permission
set eof f to 0
write d to f
close access f
return "ok""#,
        path.display()
    );
    let out = std::process::Command::new("osascript").arg("-e").arg(script).output().ok()?;
    let answer = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (answer == "ok" && path.is_file()).then_some(path)
}

#[cfg(not(target_os = "macos"))]
pub fn clipboard_png() -> Option<PathBuf> {
    None
}
