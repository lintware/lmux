//! What a workspace's processes are doing, for the sidebar card: the TCP ports
//! they listen on and the pull request for the branch. Both shell out to tools
//! the host already has (`ps`, `lsof` or `ss`, `gh`) on a background timer,
//! like `git_branch`; a missing tool just means nothing is shown.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use lmux_proto::PullRequest;

/// Run a command with a timeout; `None` on any failure.
async fn run(cmd: &str, args: &[&str], cwd: Option<&str>, timeout: Duration) -> Option<String> {
    let mut c = tokio::process::Command::new(cmd);
    c.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    let out = tokio::time::timeout(timeout, c.output()).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `ps -axo pid=,ppid=` as child lists.
pub fn parse_ps(out: &str) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        if let (Some(pid), Some(ppid)) = (it.next(), it.next()) {
            if let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    children
}

/// Every process under `roots`, the roots included.
pub fn descendants(children: &HashMap<u32, Vec<u32>>, roots: &[u32]) -> HashSet<u32> {
    let mut seen: HashSet<u32> = HashSet::new();
    let mut stack: Vec<u32> = roots.to_vec();
    while let Some(p) = stack.pop() {
        if seen.insert(p) {
            if let Some(kids) = children.get(&p) {
                stack.extend(kids.iter().copied());
            }
        }
    }
    seen
}

/// The port of a socket address like `*:3000`, `127.0.0.1:5173`, `[::1]:8080`.
fn port_of(addr: &str) -> Option<u16> {
    addr.rsplit(':').next()?.parse().ok()
}

/// `lsof -nP -iTCP -sTCP:LISTEN -F pn` (macOS): `p<pid>` then `n<addr>` lines.
pub fn parse_lsof(out: &str) -> Vec<(u32, u16)> {
    let mut pid = None;
    let mut found = Vec::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse::<u32>().ok();
        } else if let (Some(addr), Some(pid)) = (line.strip_prefix('n'), pid) {
            if let Some(port) = port_of(addr) {
                found.push((pid, port));
            }
        }
    }
    found
}

/// `ss -ltnpH` (Linux): local address in column 4, `pid=N` in the users list.
pub fn parse_ss(out: &str) -> Vec<(u32, u16)> {
    let mut found = Vec::new();
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(port) = cols.get(3).and_then(|a| port_of(a)) else { continue };
        let mut rest = line;
        while let Some(i) = rest.find("pid=") {
            rest = &rest[i + 4..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(pid) = digits.parse::<u32>() {
                found.push((pid, port));
            }
        }
    }
    found
}

/// Listening sockets on this machine as `(pid, port)`.
async fn listening() -> Vec<(u32, u16)> {
    let t = Duration::from_secs(3);
    if cfg!(target_os = "linux") {
        if let Some(out) = run("ss", &["-ltnpH"], None, t).await {
            return parse_ss(&out);
        }
    }
    run("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"], None, t)
        .await
        .map(|o| parse_lsof(&o))
        .unwrap_or_default()
}

/// For each workspace (given the pids of its panes' shells), the ports its
/// processes listen on, ascending and deduplicated.
pub async fn ports_by_workspace(roots: &HashMap<String, Vec<u32>>) -> HashMap<String, Vec<u16>> {
    let Some(ps) = run("ps", &["-axo", "pid=,ppid="], None, Duration::from_secs(3)).await else {
        return HashMap::new();
    };
    let children = parse_ps(&ps);
    let sockets = listening().await;
    roots
        .iter()
        .map(|(ws, pids)| {
            let tree = descendants(&children, pids);
            let mut ports: Vec<u16> =
                sockets.iter().filter(|(pid, _)| tree.contains(pid)).map(|(_, port)| *port).collect();
            ports.sort_unstable();
            ports.dedup();
            (ws.clone(), ports)
        })
        .collect()
}

/// The session id in a Codex rollout path:
/// `…/.codex/sessions/2026/07/11/rollout-2026-07-11T04-37-24-<uuid>.jsonl`.
pub fn codex_session_id(path: &str) -> Option<String> {
    if !path.contains("/.codex/sessions/") {
        return None;
    }
    let name = path.rsplit('/').next()?.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = name.get(name.len().checked_sub(36)?..)?;
    let ok = id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| if [8, 13, 18, 23].contains(&i) { b == b'-' } else { b.is_ascii_hexdigit() });
    ok.then(|| id.to_owned())
}

/// Files a process has open (Linux: /proc/<pid>/fd; macOS: lsof).
async fn open_files(pids: &[u32]) -> Vec<(u32, String)> {
    if pids.is_empty() {
        return Vec::new();
    }
    if cfg!(target_os = "linux") {
        let mut out = Vec::new();
        for pid in pids {
            if let Ok(mut rd) = tokio::fs::read_dir(format!("/proc/{pid}/fd")).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    if let Ok(t) = tokio::fs::read_link(e.path()).await {
                        out.push((*pid, t.to_string_lossy().into_owned()));
                    }
                }
            }
        }
        return out;
    }
    let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let Some(out) = run("lsof", &["-n", "-P", "-p", &list, "-F", "pn"], None, Duration::from_secs(3)).await else {
        return Vec::new();
    };
    let mut pid = None;
    let mut files = Vec::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(n), Some(pid)) = (line.strip_prefix('n'), pid) {
            files.push((pid, n.to_owned()));
        }
    }
    files
}

/// For each pane (by id, given its shell's pid), the Codex session running
/// in it: the rollout file a `codex` process under the pane has open.
pub async fn codex_sessions(pane_pids: &HashMap<String, u32>) -> HashMap<String, String> {
    agent_sessions(pane_pids, "codex").await
}

/// Grok keeps main and subagent files open in the same process. Prefer the
/// freshest main chat, never restore a subagent into the user's terminal.
pub async fn grok_sessions(pane_pids: &HashMap<String, u32>) -> HashMap<String, String> {
    agent_sessions(pane_pids, "grok").await
}

fn grok_chat(root: &std::path::Path, path: &str) -> Option<(String, std::path::PathBuf)> {
    let rest = std::path::Path::new(path).strip_prefix(&root).ok()?;
    let parts: Vec<_> = rest.components().collect();
    if parts.len() < 3 || !parts.iter().all(|c| matches!(c, std::path::Component::Normal(_))) { return None; }
    let id = parts[1].as_os_str().to_str()?;
    if !crate::tree::valid_agent_session_id(id) { return None; }
    let dir = root.join(parts[0]).join(id);
    // Missing or invalid metadata is not proof this is a main session.
    let summary = std::fs::read(dir.join("summary.json")).ok()?;
    let summary: serde_json::Value = serde_json::from_slice(&summary).ok()?;
    if summary["session_kind"] == "subagent" { return None; }
    Some((id.to_owned(), dir.join("chat_history.jsonl")))
}

async fn agent_sessions(pane_pids: &HashMap<String, u32>, agent: &str) -> HashMap<String, String> {
    let Some(ps) = run("ps", &["-axo", "pid=,ppid=,comm="], None, Duration::from_secs(3)).await else {
        return HashMap::new();
    };
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut names: HashMap<u32, String> = HashMap::new();
    for line in ps.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next().and_then(|p| p.parse().ok()), it.next().and_then(|p| p.parse().ok()))
        else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
        names.insert(pid, it.collect::<Vec<_>>().join(" "));
    }
    let is_codex = |pid: &u32| names.get(pid).is_some_and(|n| n.rsplit('/').next().unwrap_or(n).starts_with(agent));
    let mut found = HashMap::new();
    for (pane, root) in pane_pids {
        let codex: Vec<u32> = descendants(&children, &[*root]).into_iter().filter(|p| is_codex(p)).collect();
        if codex.is_empty() {
            continue;
        }
        let files = open_files(&codex).await;
        let id = if agent == "grok" {
            let root = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".grok/sessions");
            files.iter().filter_map(|(_, f)| grok_chat(&root, f))
                .filter_map(|(id, p)| std::fs::metadata(p).ok()?.modified().ok().map(|mtime| (mtime, id)))
                .max().map(|(_, id)| id)
        } else { files.iter().find_map(|(_, f)| codex_session_id(f)) };
        if let Some(id) = id {
            found.insert(pane.clone(), id);
        }
    }
    found
}

/// `gh pr view --json number,state,isDraft,url` output.
pub fn parse_gh_pr(out: &str) -> Option<PullRequest> {
    let v: serde_json::Value = serde_json::from_str(out).ok()?;
    let number = v.get("number")?.as_u64()? as u32;
    let draft = v.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false);
    let state = match v.get("state")?.as_str()? {
        "OPEN" if draft => "draft",
        "OPEN" => "open",
        "MERGED" => "merged",
        _ => "closed",
    };
    let url = v.get("url").and_then(|u| u.as_str()).unwrap_or_default().to_owned();
    Some(PullRequest { number, state: state.to_owned(), url })
}

/// The pull request for the branch checked out in `cwd`, if `gh` is installed
/// and logged in and one exists.
pub async fn pull_request(cwd: &str) -> Option<PullRequest> {
    let out = run(
        "gh",
        &["pr", "view", "--json", "number,state,isDraft,url"],
        Some(cwd),
        Duration::from_secs(8),
    )
    .await?;
    parse_gh_pr(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_restore_uses_only_main_sessions_inside_its_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("%2Frepo/019f4dbf-d058-78d0-a3cc-1c0f28477ca6");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("events.jsonl");
        assert!(grok_chat(root, file.to_str().unwrap()).is_none());
        std::fs::write(dir.join("summary.json"), r#"{"session_kind":"main"}"#).unwrap();
        let (id, chat) = grok_chat(root, file.to_str().unwrap()).unwrap();
        assert_eq!(id, "019f4dbf-d058-78d0-a3cc-1c0f28477ca6");
        assert_eq!(chat, dir.join("chat_history.jsonl"));
        std::fs::write(dir.join("summary.json"), r#"{"session_kind":"subagent"}"#).unwrap();
        assert!(grok_chat(root, file.to_str().unwrap()).is_none());
        assert!(grok_chat(root, "/elsewhere/events.jsonl").is_none());
        assert!(grok_chat(root, &format!("{}/%2Frepo/../x/events.jsonl", root.display())).is_none());
    }

    #[test]
    fn descendants_follow_the_process_tree() {
        let ps = "  1     0\n 10     1\n 11    10\n 12    11\n 20     1\n";
        let kids = parse_ps(ps);
        let d = descendants(&kids, &[10]);
        assert!(d.contains(&10) && d.contains(&11) && d.contains(&12));
        assert!(!d.contains(&20));
    }

    #[test]
    fn lsof_and_ss_output_parse_to_pid_port_pairs() {
        let lsof = "p501\nn*:3000\nn[::1]:3000\np777\nn127.0.0.1:5173\n";
        assert_eq!(parse_lsof(lsof), vec![(501, 3000), (501, 3000), (777, 5173)]);
        let ss = "LISTEN 0 511 0.0.0.0:8080 0.0.0.0:* users:((\"node\",pid=1234,fd=20))\n\
                  LISTEN 0 128 [::]:22 [::]:* \n\
                  LISTEN 0 511 127.0.0.1:5432 0.0.0.0:* users:((\"postgres\",pid=9,fd=5),(\"postgres\",pid=10,fd=5))\n";
        assert_eq!(parse_ss(ss), vec![(1234, 8080), (9, 5432), (10, 5432)]);
    }

    #[test]
    fn codex_rollout_paths_give_the_session_id() {
        let p = "/home/u/.codex/sessions/2026/07/11/rollout-2026-07-11T04-37-24-019f4dbf-d058-78d0-a3cc-1c0f28477ca6.jsonl";
        assert_eq!(codex_session_id(p).as_deref(), Some("019f4dbf-d058-78d0-a3cc-1c0f28477ca6"));
        assert_eq!(codex_session_id("/tmp/rollout-x.jsonl"), None);
        assert_eq!(codex_session_id("/home/u/.codex/sessions/2026/history.jsonl"), None);
    }

    #[test]
    fn gh_pr_json_maps_to_a_state() {
        let pr = parse_gh_pr(r#"{"number":42,"state":"OPEN","isDraft":true,"url":"https://x/42"}"#).unwrap();
        assert_eq!((pr.number, pr.state.as_str()), (42, "draft"));
        let pr = parse_gh_pr(r#"{"number":7,"state":"MERGED","isDraft":false,"url":"u"}"#).unwrap();
        assert_eq!(pr.state, "merged");
        assert!(parse_gh_pr("not json").is_none());
    }
}
