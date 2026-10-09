//! Chromium process lifecycle: launch a headless instance with a persistent
//! user-data-dir and discover its browser-level DevTools WebSocket endpoint.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Configuration for the managed Chromium instance.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// Chromium binary. Default `/usr/bin/chromium`.
    pub chromium_path: PathBuf,
    /// Persistent profile directory. Default `~/.local/state/lmux/chromium`.
    pub user_data_dir: PathBuf,
    /// Run with `--headless=new`. Default true.
    pub headless: bool,
    /// Extra command-line flags, appended last.
    pub extra_args: Vec<String>,
    /// Default viewport / window size used before a viewer sets its own.
    pub window_width: u32,
    pub window_height: u32,
}

/// A Chromium-family browser on this machine: the usual Linux and macOS
/// installs, then the newest Playwright download.
pub fn find_chromium() -> Option<PathBuf> {
    let fixed = [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    if let Some(p) = fixed.iter().map(PathBuf::from).find(|p| p.is_file()) {
        return Some(p);
    }
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    let cache = [home.join("Library/Caches/ms-playwright"), home.join(".cache/ms-playwright")];
    let mut dirs: Vec<PathBuf> = cache
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("chromium-")))
        .collect();
    // chromium-1243 after chromium-1187: newest first.
    dirs.sort_by_key(|p| {
        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        std::cmp::Reverse(n.trim_start_matches("chromium-").parse::<u32>().unwrap_or(0))
    });
    let inside = [
        "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
        "chrome-linux/chrome",
        "chrome-linux64/chrome",
    ];
    dirs.iter().flat_map(|d| inside.iter().map(move |i| d.join(i))).find(|p| p.is_file())
}

impl Default for BrowserConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        Self {
            chromium_path: find_chromium().unwrap_or_else(|| PathBuf::from("/usr/bin/chromium")),
            user_data_dir: PathBuf::from(home).join(".local/state/lmux/chromium"),
            headless: true,
            extra_args: Vec::new(),
            window_width: 1280,
            window_height: 800,
        }
    }
}

/// Launch Chromium and wait for its `DevTools listening on ws://…` banner.
///
/// stdout/stderr are drained by background tasks so the pipes never fill.
pub(crate) async fn launch(config: &BrowserConfig) -> anyhow::Result<(Child, String)> {
    std::fs::create_dir_all(&config.user_data_dir)?;

    let mut args: Vec<String> = vec![
        "--remote-debugging-port=0".into(),
        format!("--user-data-dir={}", config.user_data_dir.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-dev-shm-usage".into(),
        "--disable-gpu".into(),
        "--disable-background-networking".into(),
        // Keep animations/rAF running even when nothing is "focused"; a screencast
        // of a background window must still animate.
        "--disable-background-timer-throttling".into(),
        "--disable-backgrounding-occluded-windows".into(),
        "--disable-renderer-backgrounding".into(),
        "--disable-features=Translate,BackForwardCache,AcceptCHFrame,MediaRouter,OptimizationHints,CalculateNativeWinOcclusion".into(),
        "--mute-audio".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
        "--hide-crash-restore-bubble".into(),
        format!("--window-size={},{}", config.window_width, config.window_height),
    ];
    if config.headless {
        args.push("--headless=new".into());
    }
    args.extend(config.extra_args.iter().cloned());
    args.push("about:blank".into());

    let mut cmd = Command::new(&config.chromium_path);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Put Chromium in its own process group so `shutdown` can kill the whole tree
    // (browser + renderers + gpu) with one signal, without touching the daemon.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn {}: {e}", config.chromium_path.display()))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("chromium stderr not captured"))?;
    let mut lines = BufReader::new(stderr).lines();

    let ws_url = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await? {
            if let Some(idx) = line.find("ws://") {
                return Ok::<String, std::io::Error>(line[idx..].trim().to_string());
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "chromium exited before printing a DevTools endpoint",
        ))
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for the DevTools endpoint"))??;

    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut r = BufReader::new(stdout).lines();
            while let Ok(Some(_)) = r.next_line().await {}
        });
    }

    Ok((child, ws_url))
}

/// Best-effort removal of a stale singleton lock left by a previous crash.
pub(crate) fn clear_stale_lock(user_data_dir: &std::path::Path) {
    let _ = std::fs::remove_file(user_data_dir.join("SingletonLock"));
    let _ = std::fs::remove_file(user_data_dir.join("SingletonSocket"));
    let _ = std::fs::remove_file(user_data_dir.join("SingletonCookie"));
}
