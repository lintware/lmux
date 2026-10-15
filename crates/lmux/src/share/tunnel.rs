//! `cloudflared tunnel --url`: find or download the binary, start it, and read
//! the `trycloudflare.com` address it prints.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub struct Tunnel {
    pub url: String,
    // Dropping the tunnel kills the child (kill_on_drop).
    child: Child,
}

impl Tunnel {
    /// Resolves when cloudflared exits; the address is dead from then on.
    pub async fn exited(&mut self) {
        let _ = self.child.wait().await;
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Where a downloaded copy lives.
fn cache_path() -> Option<PathBuf> {
    Some(home()?.join(".local/share/lmux/bin/cloudflared"))
}

fn find_binary(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(PathBuf::from(p));
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    // Bundled beside the lmux binary.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.to_path_buf());
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(h) = home() {
        dirs.push(h.join(".local/bin"));
    }
    dirs.push("/opt/homebrew/bin".into());
    dirs.push("/usr/local/bin".into());
    dirs.into_iter().map(|d| d.join("cloudflared")).find(|p| p.is_file()).or_else(|| {
        let c = cache_path()?;
        c.is_file().then_some(c)
    })
}

/// Download cloudflared (Apache-2.0) from its GitHub releases on first use.
async fn download(print: bool) -> anyhow::Result<PathBuf> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => bail!("no cloudflared download for {other}"),
    };
    let (asset, tgz) = match os {
        "macos" => (format!("cloudflared-darwin-{arch}.tgz"), true),
        "linux" => (format!("cloudflared-linux-{arch}"), false),
        other => bail!("no cloudflared download for {other}"),
    };
    let dest = cache_path().context("no home directory")?;
    std::fs::create_dir_all(dest.parent().unwrap())?;
    if print {
        eprintln!("cloudflared not found; downloading it once to {} …", dest.display());
    }
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    let tmp = dest.with_extension("download");
    let st = Command::new("curl").env_remove("OPENAI_API_KEY").args(["-fsSL", "-o"]).arg(&tmp).arg(&url).status().await?;
    if !st.success() {
        bail!("downloading {url} failed");
    }
    if tgz {
        let dir = dest.parent().unwrap();
        let st = Command::new("tar").env_remove("OPENAI_API_KEY").arg("-xzf").arg(&tmp).arg("-C").arg(dir).status().await?;
        let _ = std::fs::remove_file(&tmp);
        if !st.success() {
            bail!("unpacking cloudflared failed");
        }
    } else {
        std::fs::rename(&tmp, &dest)?;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    Ok(dest)
}

/// Pull the quick-tunnel address out of a cloudflared log line.
pub fn parse_url(line: &str) -> Option<String> {
    let end = line.find("trycloudflare.com")? + "trycloudflare.com".len();
    let start = line[..end].rfind("https://")?;
    let url = &line[start..end];
    let host = &url["https://".len()..];
    let ok = !host.starts_with("api.")
        && host.len() > "trycloudflare.com".len() + 1
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    ok.then(|| url.to_string())
}

/// With `log`, cloudflared's own lines are copied to stderr (the gateway
/// service's journal), so a dropped tunnel leaves a reason behind.
pub async fn start(port: u16, explicit: Option<&str>, print: bool, log: bool) -> anyhow::Result<Tunnel> {
    let bin = match find_binary(explicit) {
        Some(b) => b,
        None => download(print).await?,
    };
    let mut child = Command::new(&bin)
        .env_remove("OPENAI_API_KEY")
        .args(["tunnel", "--no-autoupdate", "--url"])
        .arg(format!("http://127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))?;
    let stderr = child.stderr.take().context("no cloudflared output")?;
    let mut lines = BufReader::new(stderr).lines();

    let mut url: Option<String> = None;
    let wait = async {
        while let Some(line) = lines.next_line().await? {
            if log {
                eprintln!("cloudflared: {line}");
            }
            if url.is_none() {
                url = parse_url(&line);
            }
            // The address is printed before the edge connection is up.
            if url.is_some() && line.contains("Registered tunnel connection") {
                break;
            }
        }
        anyhow::Ok(())
    };
    let timed = tokio::time::timeout(Duration::from_secs(45), wait).await;
    let Some(url) = url else {
        bail!("cloudflared did not print a trycloudflare.com address{}", if timed.is_err() { " in 45 s" } else { "" });
    };
    // Keep draining so cloudflared never blocks on a full pipe.
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if log {
                eprintln!("cloudflared: {line}");
            }
        }
    });
    Ok(Tunnel { url, child })
}

#[cfg(test)]
mod tests {
    use super::parse_url;

    #[test]
    fn finds_the_quick_tunnel_url() {
        let l = "2026-10-11T10:00:00Z INF |  https://quiet-river-1234.trycloudflare.com                               |";
        assert_eq!(parse_url(l).as_deref(), Some("https://quiet-river-1234.trycloudflare.com"));
    }

    #[test]
    fn ignores_the_api_host_and_other_lines() {
        assert_eq!(parse_url("INF Requesting new quick Tunnel on trycloudflare.com..."), None);
        assert_eq!(parse_url("POST https://api.trycloudflare.com/tunnel"), None);
    }
}
