//! `lmux cp`: copy files and folders from this machine to a host, into a
//! workspace's working directory, so a chat running there can read them.
//!
//!   lmux cp report.docx lint-hqc-03:"ntu teaching transform"
//!   lmux cp ./data lint-hqc-03:workspace:19 --tell
//!   lmux cp notes.md lint-hqc-03:/srv/shared/
//!
//! The host is a name from `hosts.json` (or any ssh target). Copying runs
//! `scp -r` over the user's own ssh config and keys.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context};
use serde_json::Value;
use tokio::process::Command;

/// Where `host:rest` points: the host's ssh target and what follows the colon.
#[derive(Debug, PartialEq, Eq)]
pub enum Dest {
    /// `host` or `host:`: the remote home directory.
    Home,
    /// `host:/abs/path`, `host:~/x` or `host:./x`.
    Path(String),
    /// `host:<workspace title or ref>`: that workspace's working directory.
    Workspace(String),
}

/// Split `host[:rest]` at its first colon (workspace refs hold a second one).
pub fn parse_dest(s: &str) -> anyhow::Result<(String, Dest)> {
    let (host, rest) = match s.split_once(':') {
        Some((h, r)) => (h, r),
        None => (s, ""),
    };
    if host.is_empty() {
        bail!("the destination needs a host: HOST[:WORKSPACE|/PATH]");
    }
    let dest = if rest.is_empty() {
        Dest::Home
    } else if rest.starts_with('/') || rest.starts_with('~') || rest.starts_with('.') {
        Dest::Path(rest.to_string())
    } else {
        Dest::Workspace(rest.to_string())
    };
    Ok((host.to_string(), dest))
}

/// The ssh target for a host name from `hosts.json`; anything unknown is
/// passed to ssh as it is (an alias from `~/.ssh/config`).
pub fn ssh_target(name: &str, hosts: &Value) -> String {
    hosts
        .as_array()
        .into_iter()
        .flatten()
        .find(|h| ["name", "label", "ssh_target"].iter().any(|k| h[*k].as_str() == Some(name)))
        .and_then(|h| h["ssh_target"].as_str())
        .unwrap_or(name)
        .to_string()
}

pub(crate) fn hosts_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"))
    };
    Some(dir.join("lmux/hosts.json"))
}

/// The workspace a name or ref means: an exact id (`workspace:19`, `19`), an
/// exact title, or the one title that contains the text. Case is ignored.
pub fn pick_workspace<'a>(workspaces: &'a [Value], want: &str) -> anyhow::Result<&'a Value> {
    let w = want.trim();
    let id = if w.chars().all(|c| c.is_ascii_digit()) { format!("workspace:{w}") } else { w.to_string() };
    if let Some(x) = workspaces.iter().find(|x| x["id"].as_str() == Some(&id)) {
        return Ok(x);
    }
    let lower = w.to_lowercase();
    let title = |x: &Value| x["title"].as_str().unwrap_or("").to_lowercase();
    if let Some(x) = workspaces.iter().find(|x| title(x) == lower) {
        return Ok(x);
    }
    let hits: Vec<&Value> = workspaces.iter().filter(|x| title(x).contains(&lower)).collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => bail!("no workspace matches {want:?} on that host"),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|x| format!("{} ({})", x["title"].as_str().unwrap_or("?"), x["id"].as_str().unwrap_or("?")))
                .collect();
            bail!("{want:?} matches several workspaces: {}", names.join(", "))
        }
    }
}

/// Quote a path for pasting into a shell or chat prompt.
pub fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-~+@%:,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

const REMOTE_LMUX: &str = "\"$HOME/.local/bin/lmux\"";

async fn ssh(target: &str, command: &str) -> anyhow::Result<String> {
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", target, command])
        .stdin(Stdio::null())
        .output()
        .await
        .context("running ssh")?;
    if !out.status.success() {
        bail!("ssh {target}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn remote_workspaces(target: &str) -> anyhow::Result<Vec<Value>> {
    let text = ssh(target, &format!("{REMOTE_LMUX} --json list-workspaces")).await?;
    let v: Value = serde_json::from_str(&text).context("the host's lmux answered with something unreadable")?;
    Ok(v["workspaces"].as_array().cloned().unwrap_or_default())
}

pub struct Opts {
    pub sources: Vec<PathBuf>,
    pub dest: String,
    /// Paste the remote paths into the workspace's terminal (no Enter).
    pub tell: bool,
}

pub async fn run(opts: Opts) -> anyhow::Result<()> {
    if opts.sources.is_empty() {
        bail!("nothing to copy");
    }
    for s in &opts.sources {
        if !s.exists() {
            bail!("{}: no such file or folder", s.display());
        }
    }
    let (host, dest) = parse_dest(&opts.dest)?;
    let hosts: Value = hosts_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let target = ssh_target(&host, &hosts);

    let mut workspace: Option<Value> = None;
    let dir = match &dest {
        Dest::Home => None,
        Dest::Path(p) => Some(p.clone()),
        Dest::Workspace(w) => {
            let all = remote_workspaces(&target).await?;
            let ws = pick_workspace(&all, w)?.clone();
            let cwd = ws["cwd"].as_str().filter(|c| !c.is_empty()).context("that workspace has no working directory")?;
            let cwd = cwd.to_string();
            workspace = Some(ws);
            Some(cwd)
        }
    };
    if opts.tell && workspace.is_none() {
        bail!("--tell needs a workspace: HOST:WORKSPACE");
    }

    // scp -r puts a folder inside the destination folder, as cp -r does.
    let remote = format!("{target}:{}", dir.as_deref().map(|d| format!("{}/", d.trim_end_matches('/'))).unwrap_or_default());
    let status = Command::new("scp")
        .args(["-r", "-p", "-q", "-o", "BatchMode=yes"])
        .args(&opts.sources)
        .arg(&remote)
        .status()
        .await
        .context("running scp")?;
    if !status.success() {
        bail!("scp failed ({status})");
    }

    // Where each source landed, for the user and for --tell.
    let base = match &dir {
        Some(d) => d.trim_end_matches('/').to_string(),
        None => ssh(&target, "printf %s \"$HOME\"").await?,
    };
    let landed: Vec<String> = opts
        .sources
        .iter()
        .filter_map(|s| Path::new(s).file_name().map(|n| format!("{base}/{}", n.to_string_lossy())))
        .collect();
    for p in &landed {
        println!("{host}:{p}");
    }

    if opts.tell {
        let ws = workspace.as_ref().and_then(|w| w["id"].as_str()).unwrap_or_default();
        let words: Vec<String> = landed.iter().map(|p| shell_word(p)).collect();
        let text = shell_word(&words.join(" "));
        ssh(&target, &format!("{REMOTE_LMUX} paste --workspace {ws} {text}")).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn destinations() {
        assert_eq!(parse_dest("h").unwrap(), ("h".into(), Dest::Home));
        assert_eq!(parse_dest("h:").unwrap(), ("h".into(), Dest::Home));
        assert_eq!(parse_dest("h:/srv/x").unwrap().1, Dest::Path("/srv/x".into()));
        assert_eq!(parse_dest("h:~/x").unwrap().1, Dest::Path("~/x".into()));
        assert_eq!(parse_dest("h:workspace:19").unwrap().1, Dest::Workspace("workspace:19".into()));
        assert_eq!(parse_dest("h:ntu teaching").unwrap().1, Dest::Workspace("ntu teaching".into()));
        assert!(parse_dest(":x").is_err());
    }

    #[test]
    fn host_names_resolve_through_hosts_json() {
        let hosts = json!([{"name": "box", "label": "Box", "ssh_target": "me@box.example"}]);
        assert_eq!(ssh_target("box", &hosts), "me@box.example");
        assert_eq!(ssh_target("other", &hosts), "other");
        assert_eq!(ssh_target("x", &Value::Null), "x");
    }

    #[test]
    fn workspaces_match_by_ref_title_or_fragment() {
        let ws = vec![
            json!({"id": "workspace:15", "title": "ntu research (opus 5.5)", "cwd": "/a"}),
            json!({"id": "workspace:19", "title": "ntu teaching transform (opus 5.5)", "cwd": "/b"}),
        ];
        assert_eq!(pick_workspace(&ws, "19").unwrap()["cwd"], "/b");
        assert_eq!(pick_workspace(&ws, "workspace:15").unwrap()["cwd"], "/a");
        assert_eq!(pick_workspace(&ws, "NTU Teaching").unwrap()["cwd"], "/b");
        assert!(pick_workspace(&ws, "ntu").unwrap_err().to_string().contains("several"));
        assert!(pick_workspace(&ws, "nope").is_err());
    }

    #[test]
    fn paths_are_quoted_for_a_shell() {
        assert_eq!(shell_word("/a/b.txt"), "/a/b.txt");
        assert_eq!(shell_word("/a/my file.txt"), "'/a/my file.txt'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }
}
