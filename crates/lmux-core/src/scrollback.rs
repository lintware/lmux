//! Scrollback persistence for terminal surfaces.
//!
//! The daemon periodically saves each terminal's recent history (with colour
//! escapes) to `<state_dir>/scrollback/<surface>.txt`. When tmux is lost and a
//! terminal is respawned, the saved text is replayed into the new pane by a
//! small `sh` wrapper that `cat`s it and then execs the user's login shell, so
//! the old screen shows above the new prompt without being typed as input or
//! landing in shell history.

use std::path::{Path, PathBuf};

/// Lines of history captured per pane (`capture-pane -S -<N>`).
pub const CAPTURE_LINES: u32 = 2000;

/// Upper bound on one saved file. Older lines are dropped first.
pub const MAX_BYTES: usize = 1024 * 1024;

/// Environment variable naming the file the restore wrapper replays.
pub const RESTORE_ENV: &str = "LMUX_RESTORE_FILE";

/// Printed after the replayed text, before the new shell starts.
pub const RESTORE_BANNER: &str = "--- restored by lmux ---";

/// Window command for a respawned terminal. The file path comes from
/// `$LMUX_RESTORE_FILE` (set with `new-window -e`), so nothing here needs
/// quoting for the path. The file is a private copy, removed once shown, and
/// the variable is unset before the shell starts.
///
/// It is a single-quoted `sh -c` script, which parses the same way under the
/// default shells tmux may run it with (sh, bash, zsh, fish).
pub fn restore_command() -> String {
    format!(
        "sh -c 'f=${RESTORE_ENV}; unset {RESTORE_ENV}; \
         if [ -f \"$f\" ]; then cat \"$f\"; rm -f \"$f\"; \
         printf \"\\033[0m\\n{RESTORE_BANNER}\\n\"; fi; \
         exec \"${{SHELL:-/bin/sh}}\" -l'"
    )
}

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("scrollback")
}

/// File name for a surface ref: `surface:3` becomes `surface_3.txt`.
pub fn file_name(surface: &str) -> String {
    let s: String = surface
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    format!("{s}.txt")
}

pub fn path(state_dir: &Path, surface: &str) -> PathBuf {
    dir(state_dir).join(file_name(surface))
}

/// Path of the one-shot copy handed to a respawned pane.
pub fn restore_path(state_dir: &Path, surface: &str) -> PathBuf {
    path(state_dir, surface).with_extension("restore")
}

/// True for a line that shows nothing: whitespace and SGR/CSI escapes only.
fn is_blank_line(line: &str) -> bool {
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Parameters and intermediates, then one final byte.
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if !c.is_whitespace() {
            return false;
        }
    }
    true
}

/// Turn a raw capture into the file body: drop the blank rows below the last
/// output, end with a newline, and keep at most `MAX_BYTES` (cut at a line
/// boundary, keeping the newest lines).
pub fn prepare(capture: &str) -> String {
    let mut lines: Vec<&str> = capture.split('\n').collect();
    while lines.last().is_some_and(|l| is_blank_line(l)) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    if out.len() > MAX_BYTES {
        let mut cut = out.len() - MAX_BYTES;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        // Start on a whole line.
        let start = out[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(out.len());
        out = out[start..].to_string();
    }
    out
}

/// Write a prepared body atomically (temp file + rename).
pub async fn save(state_dir: &Path, surface: &str, body: &str) -> std::io::Result<()> {
    let dir = dir(state_dir);
    tokio::fs::create_dir_all(&dir).await?;
    let path = path(state_dir, surface);
    let tmp = path.with_extension("txt.tmp");
    tokio::fs::write(&tmp, body.as_bytes()).await?;
    tokio::fs::rename(&tmp, &path).await
}

/// Remove a surface's saved scrollback (and any unconsumed restore copy).
pub async fn remove(state_dir: &Path, surface: &str) {
    let _ = tokio::fs::remove_file(path(state_dir, surface)).await;
    let _ = tokio::fs::remove_file(restore_path(state_dir, surface)).await;
}

/// Copy a surface's saved scrollback to its one-shot restore path. Returns the
/// restore path, or `None` when nothing was saved.
pub async fn stage_restore(state_dir: &Path, surface: &str) -> Option<PathBuf> {
    let src = path(state_dir, surface);
    let meta = tokio::fs::metadata(&src).await.ok()?;
    if meta.len() == 0 {
        return None;
    }
    let dst = restore_path(state_dir, surface);
    tokio::fs::copy(&src, &dst).await.ok()?;
    Some(dst)
}

/// Delete saved `.txt` files whose surface no longer exists.
pub async fn prune(state_dir: &Path, keep: &[String]) {
    let keep: std::collections::HashSet<String> = keep.iter().map(|s| file_name(s)).collect();
    let Ok(mut rd) = tokio::fs::read_dir(dir(state_dir)).await else {
        return;
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".txt") && !keep.contains(&name) {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_sanitised() {
        assert_eq!(file_name("surface:3"), "surface_3.txt");
        assert_eq!(file_name("../x/y"), "___x_y.txt");
        let p = path(Path::new("/s"), "surface:3");
        assert_eq!(p, PathBuf::from("/s/scrollback/surface_3.txt"));
        assert_eq!(
            restore_path(Path::new("/s"), "surface:3"),
            PathBuf::from("/s/scrollback/surface_3.restore")
        );
    }

    #[test]
    fn prepare_trims_blank_tail() {
        assert_eq!(prepare("a\nb\n\n   \n\x1b[0m\n"), "a\nb\n");
        assert_eq!(prepare("\n\n"), "");
        assert_eq!(prepare("\x1b[31mred\x1b[0m"), "\x1b[31mred\x1b[0m\n");
    }

    #[test]
    fn prepare_caps_size_on_a_line_boundary() {
        let line = "x".repeat(99);
        let big: String = (0..20_000).map(|i| format!("{i:05}{line}\n")).collect();
        let out = prepare(&big);
        assert!(out.len() <= MAX_BYTES);
        assert!(out.ends_with(&format!("19999{line}\n")), "keeps the newest lines");
        // Starts on a whole line.
        assert_eq!(out.split('\n').next().unwrap().len(), 5 + 99);
    }

    #[tokio::test]
    async fn save_stage_remove_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path();
        save(sd, "surface:1", "hello\n").await.unwrap();
        save(sd, "surface:2", "two\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(path(sd, "surface:1")).unwrap(), "hello\n");
        let r = stage_restore(sd, "surface:1").await.unwrap();
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "hello\n");
        assert!(stage_restore(sd, "surface:9").await.is_none());

        prune(sd, &["surface:1".to_string()]).await;
        assert!(path(sd, "surface:1").exists());
        assert!(!path(sd, "surface:2").exists());

        remove(sd, "surface:1").await;
        assert!(!path(sd, "surface:1").exists());
        assert!(!r.exists());
    }

    #[test]
    fn restore_command_shape() {
        let c = restore_command();
        assert!(c.starts_with("sh -c '") && c.ends_with('\''));
        // Exactly the two outer quotes: the script never closes the quoting.
        assert_eq!(c.matches('\'').count(), 2);
        assert!(c.contains("$LMUX_RESTORE_FILE"));
        assert!(c.contains(RESTORE_BANNER));
    }
}
