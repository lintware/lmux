//! Files dropped on a pane of a remote host: copied there with `scp` over the
//! host's own ssh connection, into the workspace's working directory. The
//! pane then gets the remote paths, which the program running there can read
//! (the Mac's paths mean nothing on another machine).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};

/// What a finished copy reports: the remote paths, or why it failed.
pub type Outcome = Result<Vec<String>, String>;

/// The shared ssh transport of a host (the one the app's tunnel uses), so the
/// copy needs no new login. When it is not up, scp connects on its own.
fn ssh_opts(target: &str) -> Vec<String> {
    vec![
        "-o".into(),
        format!("ControlPath={}", crate::net::control_path(target).display()),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
    ]
}

/// Where a source ends up inside `dir`.
pub fn landed(dir: &str, source: &Path) -> Option<String> {
    let name = source.file_name()?.to_string_lossy();
    Some(format!("{}/{name}", dir.trim_end_matches('/')))
}

fn remote_dir(target: &str, dir: Option<&str>) -> Result<String, String> {
    if let Some(d) = dir.filter(|d| !d.is_empty()) {
        return Ok(d.to_owned());
    }
    let out = Command::new("ssh")
        .args(ssh_opts(target))
        .arg(target)
        .arg("pwd")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        return Err(format!("ssh {target}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn copy(target: &str, sources: &[PathBuf], dir: Option<&str>) -> Outcome {
    let dir = remote_dir(target, dir)?;
    let out = Command::new("scp")
        .args(["-r", "-p", "-q"])
        .args(ssh_opts(target))
        .args(sources)
        .arg(format!("{target}:{}/", dir.trim_end_matches('/')))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("scp: {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(if why.is_empty() { format!("scp failed ({})", out.status) } else { why });
    }
    Ok(sources.iter().filter_map(|s| landed(&dir, s)).collect())
}

/// Copy `sources` to `target:dir` (the remote home when `dir` is unknown) on a
/// background thread. `wake` repaints when it is done.
pub fn start(target: String, sources: Vec<PathBuf>, dir: Option<String>, wake: Option<egui::Context>) -> Receiver<Outcome> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(copy(&target, &sources, dir.as_deref()));
        if let Some(c) = wake {
            c.request_repaint();
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landed_paths_join_the_directory_and_name() {
        assert_eq!(landed("/srv/x/", Path::new("/Users/me/a b.txt")).as_deref(), Some("/srv/x/a b.txt"));
        assert_eq!(landed("/srv/x", Path::new("/Users/me/dir")).as_deref(), Some("/srv/x/dir"));
        assert_eq!(landed("/srv/x", Path::new("/")), None);
    }
}
