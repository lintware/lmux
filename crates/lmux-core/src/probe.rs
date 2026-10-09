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
    fn gh_pr_json_maps_to_a_state() {
        let pr = parse_gh_pr(r#"{"number":42,"state":"OPEN","isDraft":true,"url":"https://x/42"}"#).unwrap();
        assert_eq!((pr.number, pr.state.as_str()), (42, "draft"));
        let pr = parse_gh_pr(r#"{"number":7,"state":"MERGED","isDraft":false,"url":"u"}"#).unwrap();
        assert_eq!(pr.state, "merged");
        assert!(parse_gh_pr("not json").is_none());
    }
}
