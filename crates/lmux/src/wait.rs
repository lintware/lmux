//! `lmux wait-for`: wait for text on a terminal's screen, or for a named
//! signal sent by `wait-for -S` (tmux's wait-for, as cmux has it).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use lmux_proto::methods;
use serde_json::json;

use crate::call;
use crate::cli::Target;

const SIGNAL_POLL: Duration = Duration::from_millis(50);
const SCREEN_POLL: Duration = Duration::from_millis(250);

pub async fn run(
    socket: &str,
    name: Option<String>,
    signal: bool,
    text: Option<String>,
    scrollback: bool,
    target: Target,
    timeout: f64,
) -> anyhow::Result<()> {
    if !timeout.is_finite() || timeout < 0.0 {
        bail!("--timeout must be a number of seconds");
    }
    let timeout = Duration::from_secs_f64(timeout);
    match (name, text) {
        (Some(_), Some(_)) => bail!("wait-for takes a signal name or --text, not both"),
        (None, None) => bail!("wait-for requires a name (or --text PATTERN)"),
        (None, Some(_)) if signal => bail!("-S/--signal needs a name"),
        (None, Some(pattern)) => {
            wait_for_text(socket, &pattern, scrollback, &target, timeout).await?;
        }
        (Some(name), None) => {
            let dir = signal_dir()?;
            if signal {
                send_signal(&dir, &name)?;
            } else if !wait_signal(&dir, &name, timeout).await? {
                bail!("wait-for timed out waiting for '{name}'");
            }
        }
    }
    println!("OK");
    Ok(())
}

/// Poll the target's screen until `pattern` shows up.
async fn wait_for_text(
    socket: &str,
    pattern: &str,
    scrollback: bool,
    target: &Target,
    timeout: Duration,
) -> anyhow::Result<()> {
    if pattern.is_empty() {
        bail!("--text must not be empty");
    }
    let deadline = Instant::now() + timeout;
    let params = json!({
        "surface": target.surface,
        "workspace": target.workspace,
        "scrollback": scrollback,
    });
    loop {
        let v = call(socket, methods::TERM_READ_SCREEN, params.clone()).await?;
        if v["text"].as_str().is_some_and(|t| t.contains(pattern)) {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            bail!("wait-for timed out waiting for {pattern:?} on screen");
        }
        tokio::time::sleep(SCREEN_POLL.min(deadline - now)).await;
    }
}

/// A private per-user directory for signal files, so no other user can plant
/// or consume one.
fn signal_dir() -> anyhow::Result<PathBuf> {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("lmux-wait-for-{uid}"));
    ensure_private_dir(&dir)?;
    Ok(dir)
}

fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != me || meta.permissions().mode() & 0o077 != 0 {
        bail!("{} is not a private directory of this user", dir.display());
    }
    Ok(())
}

/// The signal file for `name`: hex-encoded, so distinct names never collide
/// and no name can escape the directory.
fn signal_path(dir: &Path, name: &str) -> PathBuf {
    let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{}.sig", if hex.is_empty() { "empty".into() } else { hex }))
}

fn send_signal(dir: &Path, name: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = signal_path(dir, name);
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("creating signal file {}", path.display()))?;
    Ok(())
}

/// Consume the signal, waiting up to `timeout` for it. True once taken.
async fn wait_signal(dir: &Path, name: &str, timeout: Duration) -> anyhow::Result<bool> {
    let path = signal_path(dir, name);
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("taking signal {}", path.display())),
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(SIGNAL_POLL.min(deadline - now)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names_map_to_distinct_safe_files() {
        let dir = Path::new("/d");
        assert_ne!(signal_path(dir, "a/b"), signal_path(dir, "a_b"));
        assert_eq!(signal_path(dir, "../x").parent(), Some(dir));
        assert_eq!(signal_path(dir, ""), dir.join("empty.sig"));
    }

    #[tokio::test]
    async fn a_signal_sent_first_wakes_one_waiter() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sig");
        ensure_private_dir(&dir).unwrap();
        assert!(!wait_signal(&dir, "build", Duration::from_millis(60)).await.unwrap());
        send_signal(&dir, "build").unwrap();
        assert!(wait_signal(&dir, "build", Duration::from_millis(60)).await.unwrap());
        // Consumed: a second waiter times out.
        assert!(!wait_signal(&dir, "build", Duration::from_millis(60)).await.unwrap());

        // A signal sent while waiting wakes the waiter.
        let d2 = dir.clone();
        let waiter = tokio::spawn(async move { wait_signal(&d2, "late", Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        send_signal(&dir, "late").unwrap();
        assert!(waiter.await.unwrap().unwrap());
    }
}
