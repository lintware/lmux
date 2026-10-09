//! `lmux import-cmux`: rebuild cmux's workspaces in lmux and, with `--move`,
//! hand their processes over.
//!
//! A move goes one workspace at a time: build it in lmux (on this Mac, or over
//! ssh on the host a remote cmux workspace ran on), close it in cmux, wait for
//! its Claude sessions to exit, then resume each one in its lmux tab. A Claude
//! that is mid-turn is left in cmux unless `--force`, so no work is cut off,
//! and the cmux workspace this command runs in is never closed under it.

use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use lmux_proto::cmux::{from_cmux_export, CmuxWorkspace};
use lmux_proto::methods;
use serde::Serialize;
use serde_json::{json, Value};

pub struct Opts {
    pub from: Option<String>,
    pub move_: bool,
    pub force: bool,
    pub dry_run: bool,
    pub only: Vec<String>,
}

#[derive(Serialize)]
struct Outcome {
    title: String,
    host: String,
    /// "moved", "copied", "planned" or "skipped".
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    tabs: usize,
    agents: usize,
    agents_resumed: usize,
}

fn cmux_bin() -> String {
    std::env::var("CMUX_BUNDLED_CLI_PATH")
        .ok()
        .filter(|p| std::path::Path::new(p).exists())
        .unwrap_or_else(|| {
            let app = "/Applications/cmux.app/Contents/Resources/bin/cmux";
            if std::path::Path::new(app).exists() { app.into() } else { "cmux".into() }
        })
}

fn cmux(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(cmux_bin())
        .args(args)
        .env("CMUX_QUIET", "1")
        .output()
        .context("running cmux (is cmux installed?)")?;
    if !out.status.success() {
        bail!("cmux {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn export(from: Option<&str>) -> anyhow::Result<Value> {
    let text = match from {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
        None => {
            let dir = std::env::temp_dir().join(format!("lmux-cmux-import-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("session.json");
            cmux(&["restore-session", "--export", &path.to_string_lossy(), "--force"])?;
            let text = std::fs::read_to_string(&path)?;
            let _ = std::fs::remove_dir_all(&dir);
            text
        }
    };
    serde_json::from_str(&text).context("reading the cmux export")
}

const REMOTE_LMUX: &str = "~/.local/bin/lmux";

fn ssh(host: &str, cmd: &str) -> anyhow::Result<std::process::Output> {
    Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", host, cmd])
        .output()
        .with_context(|| format!("ssh {host}"))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Call a method on the daemon of `host` (None: this Mac's).
async fn call_on(host: Option<&str>, socket: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    match host {
        None => crate::call(socket, method, params).await,
        Some(h) => {
            let cmd = format!("{REMOTE_LMUX} --json rpc {method} {}", sh_quote(&params.to_string()));
            let out = ssh(h, &cmd)?;
            if !out.status.success() {
                bail!("{h}: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
        }
    }
}

fn host_has_lmux(host: &str) -> bool {
    ssh(host, &format!("test -x {REMOTE_LMUX}")).map(|o| o.status.success()).unwrap_or(false)
}

/// cmux's sidebar shows a Claude that is mid-turn as `claude_code=Running`.
fn agent_running(cmux_id: &str) -> bool {
    cmux(&["sidebar-state", "--workspace", cmux_id])
        .map(|s| s.lines().any(|l| l.trim().starts_with("claude_code=") && l.to_lowercase().contains("running")))
        .unwrap_or(false)
}

/// Wait until no process on `host` carries one of these session ids, so a
/// session is never run by two Claudes at once.
fn wait_agents_gone(host: Option<&str>, ids: &[String], limit: Duration) -> bool {
    let start = Instant::now();
    loop {
        let alive = ids.iter().any(|id| match host {
            None => Command::new("pgrep").args(["-f", id]).output().map(|o| o.status.success()).unwrap_or(true),
            Some(h) => ssh(h, &format!("pgrep -f {}", sh_quote(id))).map(|o| o.status.success()).unwrap_or(true),
        });
        if !alive {
            return true;
        }
        if start.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub async fn run(opts: Opts, socket: &str, json_out: bool) -> anyhow::Result<()> {
    let export = export(opts.from.as_deref())?;
    let mut all = from_cmux_export(&export);
    if !opts.only.is_empty() {
        all.retain(|w| opts.only.iter().any(|o| o == &w.cmux_id || o == &w.spec.title));
    }
    let caller = std::env::var("CMUX_WORKSPACE_ID").ok();
    let mut outcomes = vec![];
    let mut first_local: Option<String> = None;
    let mut hosts_ok: std::collections::HashMap<String, bool> = Default::default();

    for w in &all {
        let o = import_one(w, &opts, socket, caller.as_deref(), &mut hosts_ok).await;
        if !json_out {
            print_outcome(&o);
        }
        if w.host.is_none() && first_local.is_none() && matches!(o.status, "moved" | "copied") {
            first_local = o.workspace.clone();
        }
        outcomes.push(o);
    }
    // Show what came over.
    if let Some(ws) = &first_local {
        let _ = crate::call(socket, methods::WORKSPACE_SELECT, json!({"workspace": ws})).await;
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({"workspaces": outcomes}))?);
    } else {
        let n = |s: &str| outcomes.iter().filter(|o| o.status == s).count();
        println!(
            "\n{} moved, {} copied, {} skipped{}",
            n("moved"),
            n("copied"),
            n("skipped"),
            if opts.dry_run { " (dry run: nothing changed)" } else { "" }
        );
    }
    Ok(())
}

async fn import_one(
    w: &CmuxWorkspace,
    opts: &Opts,
    socket: &str,
    caller: Option<&str>,
    hosts_ok: &mut std::collections::HashMap<String, bool>,
) -> Outcome {
    let tabs = w.spec.layout.tabs();
    let ids: Vec<String> = tabs.iter().filter_map(|t| t.agent.as_ref().map(|a| a.id.clone())).collect();
    let mut o = Outcome {
        title: w.spec.title.clone(),
        host: w.host.clone().unwrap_or_else(|| "this mac".into()),
        status: "skipped",
        reason: None,
        workspace: None,
        tabs: tabs.len(),
        agents: ids.len(),
        agents_resumed: 0,
    };
    if let Some(h) = &w.host {
        let ok = *hosts_ok.entry(h.clone()).or_insert_with(|| host_has_lmux(h));
        if !ok {
            o.reason = Some(format!("lmux is not installed on {h} (deploy/install-remote.sh {h})"));
            return o;
        }
    }
    if opts.move_ && caller == Some(w.cmux_id.as_str()) {
        o.reason = Some("this command is running in it; run the import from another workspace to move it".into());
        return o;
    }
    if opts.move_ && !opts.force && !ids.is_empty() && agent_running(&w.cmux_id) {
        o.reason = Some("its Claude is working; run again when it is idle, or pass --force".into());
        return o;
    }
    if opts.dry_run {
        o.status = "planned";
        return o;
    }
    let host = w.host.as_deref();
    let made = match call_on(host, socket, methods::WORKSPACE_IMPORT, json!({"workspace": w.spec})).await {
        Ok(v) => v,
        Err(e) => {
            o.reason = Some(format!("{e:#}"));
            return o;
        }
    };
    o.workspace = made["workspace"].as_str().map(str::to_string);
    if !opts.move_ {
        // cmux keeps running these sessions, so they are only tagged here.
        o.status = "copied";
        return o;
    }
    if let Err(e) = cmux(&["close-workspace", "--workspace", &w.cmux_id]) {
        o.status = "copied";
        o.reason = Some(format!("built in lmux, but cmux did not close it: {e:#}"));
        return o;
    }
    o.status = "moved";
    if !ids.is_empty() && !wait_agents_gone(host, &ids, Duration::from_secs(20)) {
        o.reason = Some("Claude was still running after cmux closed the workspace; not resumed".into());
        return o;
    }
    for a in made["agents"].as_array().into_iter().flatten() {
        let (Some(surface), Some(cmd)) = (a["surface"].as_str(), a["command"].as_str()) else { continue };
        let sent = call_on(host, socket, methods::TERM_SEND, json!({"surface": surface, "text": format!("{cmd}\n")})).await;
        if sent.is_ok() {
            o.agents_resumed += 1;
        }
    }
    o
}

fn print_outcome(o: &Outcome) {
    let on = if o.host == "this mac" { String::new() } else { format!(" on {}", o.host) };
    let what = match o.status {
        "moved" | "copied" | "planned" => {
            let agents = if o.agents == 0 {
                String::new()
            } else if o.status == "moved" {
                format!(", {}/{} Claude resumed", o.agents_resumed, o.agents)
            } else {
                format!(", {} Claude", o.agents)
            };
            format!("{} {} tab(s){agents}", o.workspace.as_deref().unwrap_or(""), o.tabs)
        }
        _ => String::new(),
    };
    println!("{:<8} {}{on}  {what}", o.status, o.title);
    if let Some(r) = &o.reason {
        println!("         {r}");
    }
}
