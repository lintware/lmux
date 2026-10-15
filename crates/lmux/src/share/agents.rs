//! Which agent runs in each shared terminal, and its transcript as chat
//! messages. Transcript paths are worked out here from the daemon's data
//! (`term.processes`) and the local process table; the page never sends a
//! path. Transcript contents are never logged.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Only the end of a transcript is read.
const TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Messages per `chat` frame.
const MAX_MESSAGES: usize = 200;
/// Longer messages are cut.
const MAX_MESSAGE_BYTES: usize = 8 * 1024;
/// A transcript that was not found is looked for again after this long.
const MISS_TTL: Duration = Duration::from_secs(15);
/// How long the open files of an agent process are trusted.
const OPEN_TTL: Duration = Duration::from_secs(10);

/// What the share knows about one terminal tab.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TabAgent {
    /// `claude`, `codex`, `grok`, `shell` or `other`; `None` when unknown.
    pub agent: Option<&'static str>,
    /// tmux `pane_current_command`.
    pub command: Option<String>,
    pub transcript: Option<PathBuf>,
}

/// Files each process has open.
type OpenFiles = HashMap<u32, Vec<String>>;

/// Finds agents and their transcripts, caching the lookups between polls.
pub struct Finder {
    home: PathBuf,
    by_tag: HashMap<String, (Instant, Option<PathBuf>)>,
    /// Open files for the last set of agent pids looked at.
    open: Option<(Instant, Vec<u32>, OpenFiles)>,
}

impl Finder {
    pub fn new(home: PathBuf) -> Self {
        Finder { home, by_tag: HashMap::new(), open: None }
    }

    /// `terminals` is the daemon's `term.processes` result.
    pub async fn detect(&mut self, terminals: &Value) -> HashMap<String, TabAgent> {
        let list = terminals["terminals"].as_array().cloned().unwrap_or_default();
        let procs = if list.is_empty() { Procs::default() } else { Procs::load().await };
        // First what runs where, then one open-files lookup for every Codex
        // and Grok process at once.
        let mut found = Vec::new();
        for t in &list {
            let Some(surface) = t["surface"].as_str() else { continue };
            let command = t["command"].as_str().map(str::to_owned);
            let tag = t["agent_session"].as_str();
            let pid = t["pid"].as_u64().and_then(|p| u32::try_from(p).ok()).filter(|p| *p > 1);
            let below = pid.map(|p| procs.tree(p)).unwrap_or_default();
            let names: Vec<&str> = below.iter().filter_map(|p| procs.names.get(p).map(String::as_str)).collect();
            let agent = classify(tag, command.as_deref(), &names);
            // The tag only counts for the agent actually detected.
            let id = tag
                .and_then(|t| t.split_once(':'))
                .filter(|(a, id)| Some(*a) == agent && valid_id(id))
                .map(|(_, id)| id.to_owned());
            let agent_pids: Vec<u32> = if matches!(agent, Some("codex" | "grok")) {
                below.iter().copied().filter(|p| procs.names.get(p).and_then(|n| agent_of_name(n)) == agent).collect()
            } else {
                Vec::new()
            };
            let cwd = t["cwd"].as_str().map(str::to_owned);
            found.push((surface.to_owned(), command, agent, id, agent_pids, cwd));
        }
        let all: Vec<u32> = found.iter().flat_map(|f| f.4.iter().copied()).collect();
        let open = self.open_files(all).await;
        let mut out = HashMap::new();
        for (surface, command, agent, id, agent_pids, cwd) in found {
            let files: Vec<String> =
                agent_pids.iter().filter_map(|p| open.get(p)).flatten().cloned().collect();
            let transcript = match agent {
                // Without a tag (no SessionStart hook yet), the newest session
                // of the pane's directory.
                Some("claude") => match (id, cwd) {
                    (Some(id), _) => self.by_tag("claude", id).await,
                    (None, Some(cwd)) => {
                        let home = self.home.clone();
                        tokio::task::spawn_blocking(move || claude_newest(&home, &cwd)).await.ok().flatten()
                    }
                    _ => None,
                },
                // The rollout the codex process has open is the freshest; the
                // daemon's tag (refreshed every 30 s) covers the rest.
                Some("codex") => match (codex_open(&self.home, &files), id) {
                    (Some(p), _) => Some(p),
                    (None, Some(id)) => self.by_tag("codex", id).await,
                    _ => None,
                },
                Some("grok") => {
                    let home = self.home.clone();
                    tokio::task::spawn_blocking(move || grok_session(&home, &files)).await.ok().flatten()
                }
                _ => None,
            };
            out.insert(surface, TabAgent { agent, command, transcript });
        }
        out
    }

    async fn by_tag(&mut self, agent: &'static str, id: String) -> Option<PathBuf> {
        let key = format!("{agent}:{id}");
        if let Some((at, hit)) = self.by_tag.get(&key) {
            if hit.as_ref().is_some_and(|p| p.is_file()) || (hit.is_none() && at.elapsed() < MISS_TTL) {
                return hit.clone();
            }
        }
        let home = self.home.clone();
        let hit = tokio::task::spawn_blocking(move || match agent {
            "claude" => claude_path(&home, &id),
            _ => codex_path(&home, &id),
        })
        .await
        .ok()
        .flatten();
        self.by_tag.insert(key, (Instant::now(), hit.clone()));
        hit
    }

    /// Files the given processes have open, by pid. The same pid set is
    /// trusted for `OPEN_TTL`; a new or exited agent process looks again.
    async fn open_files(&mut self, mut pids: Vec<u32>) -> OpenFiles {
        pids.sort_unstable();
        pids.dedup();
        if pids.is_empty() {
            return HashMap::new();
        }
        if let Some((at, key, files)) = &self.open {
            if at.elapsed() < OPEN_TTL && *key == pids {
                return files.clone();
            }
        }
        let files = list_open_files(&pids).await;
        self.open = Some((Instant::now(), pids, files.clone()));
        files
    }
}

/// Files the processes have open (Linux: /proc/<pid>/fd; elsewhere: one lsof).
async fn list_open_files(pids: &[u32]) -> OpenFiles {
    let mut out = OpenFiles::new();
    if cfg!(target_os = "linux") {
        let pids = pids.to_vec();
        return tokio::task::spawn_blocking(move || {
            for pid in pids {
                let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
                for e in rd.flatten() {
                    if let Ok(t) = std::fs::read_link(e.path()) {
                        out.entry(pid).or_default().push(t.to_string_lossy().into_owned());
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default();
    }
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let Some(text) = run("lsof", &["-n", "-P", "-p", &list, "-F", "pn"]).await else { return out };
    let mut pid = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(n), Some(p)) = (line.strip_prefix('n'), pid) {
            out.entry(p).or_default().push(n.to_owned());
        }
    }
    out
}

/// The agent behind a terminal: a foreground command named after an agent
/// first (a stale session tag must not hide a new agent), then the session
/// tag (Claude's command is just its version), then the processes under the
/// pane, then shell or other.
pub fn classify(tag: Option<&str>, command: Option<&str>, below: &[&str]) -> Option<&'static str> {
    if let Some(a) = command.and_then(agent_of_name) {
        return Some(a);
    }
    if let Some((agent, _)) = tag.and_then(|t| t.split_once(':')) {
        match agent {
            "claude" => return Some("claude"),
            "codex" => return Some("codex"),
            _ => {}
        }
    }
    if let Some(a) = below.iter().find_map(|n| agent_of_name(n)) {
        return Some(a);
    }
    let command = command?;
    Some(if is_shell(command) { "shell" } else { "other" })
}

fn agent_of_name(name: &str) -> Option<&'static str> {
    let base = name.rsplit('/').next().unwrap_or(name).trim_start_matches('-').to_ascii_lowercase();
    ["claude", "codex", "grok"].into_iter().find(|a| base.starts_with(a))
}

fn is_shell(command: &str) -> bool {
    let base = command.rsplit('/').next().unwrap_or(command);
    matches!(
        base.trim_start_matches('-'),
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh" | "nu"
    )
}

/// Agent session ids are letters, digits and dashes, so they are safe in a path.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `~/.claude/projects/*/<id>.jsonl`.
pub fn claude_path(home: &Path, id: &str) -> Option<PathBuf> {
    if !valid_id(id) {
        return None;
    }
    let name = format!("{id}.jsonl");
    std::fs::read_dir(home.join(".claude/projects"))
        .ok()?
        .flatten()
        .map(|d| d.path().join(&name))
        .find(|p| p.is_file())
}

/// The most recently written `~/.claude/projects/<cwd>/*.jsonl`, where Claude
/// names the directory after the cwd with every other character a dash.
pub fn claude_newest(home: &Path, cwd: &str) -> Option<PathBuf> {
    if !cwd.starts_with('/') {
        return None;
    }
    let name: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    std::fs::read_dir(home.join(".claude/projects").join(name))
        .ok()?
        .flatten()
        .filter(|d| {
            d.file_name().to_str().is_some_and(|n| n.strip_suffix(".jsonl").is_some_and(valid_id))
        })
        .filter_map(|d| Some((d.metadata().ok()?.modified().ok()?, d.path())))
        .max()
        .map(|(_, p)| p)
}

/// `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<id>.jsonl`, newest days first.
pub fn codex_path(home: &Path, id: &str) -> Option<PathBuf> {
    if !valid_id(id) {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    let sorted = |dir: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|r| r.flatten().map(|d| d.path()).collect()).unwrap_or_default();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    };
    for year in sorted(&home.join(".codex/sessions")) {
        for month in sorted(&year) {
            for day in sorted(&month) {
                let hit = sorted(&day).into_iter().find(|f| {
                    f.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
                });
                if hit.is_some() {
                    return hit;
                }
            }
        }
    }
    None
}

/// The rollout a codex process has open, if it is one of `~/.codex/sessions`.
pub fn codex_open(home: &Path, open_files: &[String]) -> Option<PathBuf> {
    let root = home.join(".codex/sessions");
    open_files.iter().map(Path::new).find_map(|p| {
        let rest = p.strip_prefix(&root).ok()?;
        let name = p.file_name()?.to_str()?;
        let ok = rest.components().all(|c| matches!(c, Component::Normal(_)))
            && name.starts_with("rollout-")
            && name.ends_with(".jsonl");
        ok.then(|| p.to_path_buf())
    })
}

/// Of the Grok sessions whose files a process has open
/// (`~/.grok/sessions/<cwd>/<id>/…`), the one whose chat changed last.
/// Subagent sessions run in the same process and are open too; they are
/// only used when no main session is.
pub fn grok_session(home: &Path, open_files: &[String]) -> Option<PathBuf> {
    let root = home.join(".grok/sessions");
    let mut seen = std::collections::HashSet::new();
    // (is main session, chat mtime, chat path); the largest wins.
    let mut best: Option<(bool, std::time::SystemTime, PathBuf)> = None;
    for f in open_files {
        let Ok(rest) = Path::new(f).strip_prefix(&root) else { continue };
        let mut parts = rest.components();
        let (Some(Component::Normal(cwd)), Some(Component::Normal(id)), Some(_)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !id.to_str().is_some_and(valid_id) || !seen.insert((cwd.to_owned(), id.to_owned())) {
            continue;
        }
        let dir = root.join(cwd).join(id);
        let chat = dir.join("chat_history.jsonl");
        let Ok(mtime) = std::fs::metadata(&chat).and_then(|m| m.modified()) else { continue };
        let main = !grok_is_subagent(&dir);
        let better = match &best {
            None => true,
            Some((m, t, _)) => (main, mtime) > (*m, *t),
        };
        if better {
            best = Some((main, mtime, chat));
        }
    }
    best.map(|(_, _, p)| p)
}

/// `summary.json` says `"session_kind": "subagent"` for a subagent's session.
fn grok_is_subagent(dir: &Path) -> bool {
    let Ok(f) = std::fs::File::open(dir.join("summary.json")) else { return false };
    let mut buf = Vec::new();
    if f.take(256 * 1024).read_to_end(&mut buf).is_err() {
        return false;
    }
    serde_json::from_slice::<Value>(&buf).is_ok_and(|v| v["session_kind"] == "subagent")
}

/// The last messages of a transcript as `chat` frame entries.
pub fn read_chat(agent: &str, path: &Path) -> std::io::Result<Vec<Value>> {
    let text = tail(path)?;
    let msgs = match agent {
        "claude" => parse_claude(&text),
        "codex" => parse_codex(&text),
        "grok" => parse_grok(&text),
        _ => Vec::new(),
    };
    let skip = msgs.len().saturating_sub(MAX_MESSAGES);
    Ok(msgs.into_iter().skip(skip).map(|m| json!({"role": m.role, "text": m.text, "ts": m.ts})).collect())
}

/// The last `TAIL_BYTES` of a file, starting at a whole line.
fn tail(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 { text.split_once('\n').map(|(_, r)| r.to_owned()).unwrap_or_default() } else { text })
}

#[derive(Debug, PartialEq)]
pub struct Msg {
    pub role: &'static str,
    pub text: String,
    pub ts: Option<String>,
}

fn push(out: &mut Vec<Msg>, role: &'static str, text: &str, ts: Option<&str>) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    out.push(Msg { role, text: cap(text), ts: ts.map(str::to_owned) });
}

fn cap(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn one_line(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        s
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn starts_with_any(s: &str, prefixes: &[&str]) -> bool {
    let s = s.trim_start();
    prefixes.iter().any(|p| s.starts_with(p))
}

fn lines(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok())
}

/// Claude Code: `user` and `assistant` lines; text and tool calls, no
/// tool results, thinking, meta or sidechain lines.
pub fn parse_claude(text: &str) -> Vec<Msg> {
    const NOISE: &[&str] = &[
        "<command-name>",
        "<command-message>",
        "<local-command-",
        "<system-reminder>",
        "<bash-stdout>",
        "<bash-stderr>",
    ];
    let mut out = Vec::new();
    for o in lines(text) {
        let role = match o["type"].as_str() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        let flag = |k: &str| o[k].as_bool() == Some(true);
        if flag("isMeta") || flag("isSidechain") || flag("isCompactSummary") {
            continue;
        }
        let ts = o["timestamp"].as_str();
        match &o["message"]["content"] {
            // `!cmd` typed at the prompt.
            Value::String(s) if s.trim_start().starts_with("<bash-input>") => {
                let cmd = s.trim().trim_start_matches("<bash-input>").trim_end_matches("</bash-input>");
                push(&mut out, "tool", &format!("$ {}", one_line(cmd, 120)), ts);
            }
            Value::String(s) if !starts_with_any(s, NOISE) => push(&mut out, role, s, ts),
            Value::Array(blocks) => {
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => {
                            let s = b["text"].as_str().unwrap_or("");
                            if !starts_with_any(s, NOISE) {
                                push(&mut out, role, s, ts);
                            }
                        }
                        Some("tool_use") => {
                            let name = b["name"].as_str().unwrap_or("tool");
                            push(&mut out, "tool", &tool_line(name, &b["input"]), ts);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `Name · hint`, the hint taken from the first telling input field.
fn tool_line(name: &str, input: &Value) -> String {
    const KEYS: &[&str] = &[
        "command",
        "file_path",
        "target_file",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
        "skill",
    ];
    match KEYS.iter().find_map(|k| input[*k].as_str()) {
        Some(hint) => format!("{name} · {}", one_line(hint, 120)),
        None => name.to_owned(),
    }
}

fn block_texts(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Codex: completed items from `event_msg` lines only (`response_item` lines
/// repeat them).
pub fn parse_codex(text: &str) -> Vec<Msg> {
    const NOISE: &[&str] = &["<environment_context>", "<user_instructions>", "<developer", "# AGENTS.md"];
    let mut out = Vec::new();
    for o in lines(text) {
        if o["type"] != "event_msg" || o["payload"]["type"] != "item_completed" {
            continue;
        }
        let item = &o["payload"]["item"];
        let ts = o["timestamp"].as_str();
        match item["type"].as_str() {
            Some("UserMessage") => {
                let s = block_texts(&item["content"]);
                if !starts_with_any(&s, NOISE) {
                    push(&mut out, "user", &s, ts);
                }
            }
            Some("AgentMessage") => push(&mut out, "assistant", &block_texts(&item["content"]), ts),
            Some("CommandExecution") => {
                let cmd = match &item["command"] {
                    Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
                    v => v.as_str().unwrap_or("").to_owned(),
                };
                let mut line = format!("$ {}", one_line(&cmd, 120));
                if let Some(code) = item["exit_code"].as_i64().filter(|c| *c != 0) {
                    line.push_str(&format!(" · exit {code}"));
                }
                push(&mut out, "tool", &line, ts);
            }
            Some("FileChange") => {
                let files: Vec<&str> = item["changes"]
                    .as_object()
                    .map(|m| m.keys().map(|k| k.rsplit('/').next().unwrap_or(k)).collect())
                    .unwrap_or_default();
                push(&mut out, "tool", &format!("Edited {}", one_line(&files.join(", "), 120)), ts);
            }
            Some("McpToolCall") => {
                let name = [item["server"].as_str(), item["tool"].as_str()].into_iter().flatten().collect::<Vec<_>>();
                push(&mut out, "tool", &name.join("."), ts);
            }
            _ => {}
        }
    }
    out
}

/// Grok: `user` and `assistant` lines; the query inside `<user_query>`, and
/// tool call names.
pub fn parse_grok(text: &str) -> Vec<Msg> {
    const NOISE: &[&str] = &["<system-reminder>", "<user_info>", "<rules>"];
    let mut out = Vec::new();
    let mut calls = HashMap::new();
    for o in lines(text) {
        let ts = o["timestamp"].as_str().or_else(|| o["created_at"].as_str());
        match o["type"].as_str() {
            Some("user") => {
                let parts: Vec<String> = match &o["content"] {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str().map(str::to_owned)).collect(),
                    _ => Vec::new(),
                };
                let text: Vec<String> = parts
                    .iter()
                    .filter_map(|p| match (p.find("<user_query>"), p.find("</user_query>")) {
                        (Some(a), Some(b)) if b > a => Some(p[a + "<user_query>".len()..b].to_owned()),
                        _ if starts_with_any(p, NOISE) => None,
                        _ => Some(p.clone()),
                    })
                    .collect();
                push(&mut out, "user", &text.join("\n\n"), ts);
            }
            Some("assistant") => {
                push(&mut out, "assistant", &block_texts(&o["content"]), ts);
                for c in o["tool_calls"].as_array().map(Vec::as_slice).unwrap_or_default() {
                    if let Some(name) = c["name"].as_str().or_else(|| c["function"]["name"].as_str()) {
                        // `arguments` is a JSON object encoded as a string.
                        let args = c.get("arguments").or_else(|| c["function"].get("arguments")).unwrap_or(&Value::Null);
                        let input = match args {
                            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                            other => other.clone(),
                        };
                        if let Some(id) = c["id"].as_str() { calls.insert(id.to_owned(), name.to_owned()); }
                        push(&mut out, "tool", &tool_line(name, &input), ts);
                    }
                }
            }
            Some("tool_result" | "tool") => {
                let id = o["tool_call_id"].as_str().or_else(|| o["tool_use_id"].as_str());
                let name = o["name"].as_str().or_else(|| id.and_then(|id| calls.get(id).map(String::as_str)));
                if let Some(name) = name {
                    let output = block_texts(&o["content"]);
                    if !output.trim().is_empty() {
                        let status = if o["is_error"] == true { "failed" } else { "output" };
                        push(&mut out, "tool", &format!("{name} · {status}\n{output}"), ts);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The process table: names, and children by parent.
#[derive(Default)]
struct Procs {
    names: HashMap<u32, String>,
    children: HashMap<u32, Vec<u32>>,
}

impl Procs {
    async fn load() -> Procs {
        let mut p = Procs::default();
        let Some(ps) = run("ps", &["-axo", "pid=,ppid=,comm="]).await else { return p };
        for line in ps.lines() {
            let mut it = line.split_whitespace();
            let (Some(pid), Some(ppid)) =
                (it.next().and_then(|v| v.parse().ok()), it.next().and_then(|v| v.parse().ok()))
            else {
                continue;
            };
            p.children.entry(ppid).or_default().push(pid);
            p.names.insert(pid, it.collect::<Vec<_>>().join(" "));
        }
        p
    }

    /// `root` and everything under it.
    fn tree(&self, root: u32) -> Vec<u32> {
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() && out.len() < 512 {
            if let Some(kids) = self.children.get(&out[i]) {
                out.extend(kids.iter().copied().filter(|k| *k != root));
            }
            i += 1;
        }
        out
    }
}

async fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(cmd).env_remove("OPENAI_API_KEY").args(args).stdin(std::process::Stdio::null()).output(),
    )
    .await
    .ok()?
    .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_are_classified_in_order() {
        assert_eq!(classify(Some("claude:abc"), Some("2.1.296"), &[]), Some("claude"));
        assert_eq!(classify(Some("codex:abc"), Some("zsh"), &[]), Some("codex"));
        assert_eq!(classify(None, Some("grok-1.0.50"), &[]), Some("grok"));
        assert_eq!(classify(None, Some("2.1.296"), &["-zsh", "/usr/local/bin/claude"]), Some("claude"));
        assert_eq!(classify(None, Some("zsh"), &["zsh"]), Some("shell"));
        assert_eq!(classify(None, Some("vim"), &["vim"]), Some("other"));
        assert_eq!(classify(None, None, &[]), None);
        // A Claude tag left on the window does not hide a Grok started after it.
        assert_eq!(classify(Some("claude:abc"), Some("grok-1.0.50"), &[]), Some("grok"));
        assert_eq!(classify(None, Some("zsh"), &["-zsh", "/Users/x/.codex/bin/codex"]), Some("codex"));
    }

    #[test]
    fn claude_lines_become_messages() {
        let t = [
            r#"{"type":"user","timestamp":"t1","message":{"content":"fix the build"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"meta"}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"assistant","timestamp":"t2","message":{"content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"On it."},{"type":"tool_use","name":"Bash","input":{"command":"cargo  build\n-p lmux"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":"side"}}"#,
            r#"{"type":"system","message":{"content":"sys"}}"#,
            r#"{"type":"user","isCompactSummary":true,"message":{"content":"summary"}}"#,
            r#"{"type":"user","message":{"content":"<bash-input>ls -la</bash-input>"}}"#,
            r#"{"type":"user","message":{"content":"<bash-stdout>secret</bash-stdout><bash-stderr></bash-stderr>"}}"#,
            "not json",
        ]
        .join("\n");
        let m = parse_claude(&t);
        assert_eq!(
            m,
            vec![
                Msg { role: "user", text: "fix the build".into(), ts: Some("t1".into()) },
                Msg { role: "assistant", text: "On it.".into(), ts: Some("t2".into()) },
                Msg { role: "tool", text: "Bash · cargo build -p lmux".into(), ts: Some("t2".into()) },
                Msg { role: "tool", text: "$ ls -la".into(), ts: None },
            ]
        );
    }

    #[test]
    fn codex_items_become_messages() {
        let t = [
            r#"{"timestamp":"t1","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"add a test"}]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"<environment_context>x</environment_context>"}]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","content":[{"text":"think"}]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["cargo","test"],"exit_code":101}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","changes":{"/a/src/lib.rs":{}}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"McpToolCall","server":"web","tool":"search","arguments":{}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"Done."}]}}}"#,
            r#"{"type":"response_item","payload":{"type":"message","content":[{"text":"dup"}]}}"#,
        ]
        .join("\n");
        let m: Vec<(&str, String)> = parse_codex(&t).into_iter().map(|m| (m.role, m.text)).collect();
        assert_eq!(
            m,
            vec![
                ("user", "add a test".into()),
                ("tool", "$ cargo test · exit 101".into()),
                ("tool", "Edited lib.rs".into()),
                ("tool", "web.search".into()),
                ("assistant", "Done.".into()),
            ]
        );
    }

    #[test]
    fn grok_lines_become_messages() {
        let t = [
            r#"{"type":"system","content":"you are grok"}"#,
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>os</user_info>"},{"type":"text","text":"<user_query>\nship it\n</user_query>"}]}"#,
            r#"{"type":"reasoning","content":"hm"}"#,
            r#"{"type":"assistant","content":"Shipping.","tool_calls":[{"name":"run_terminal_cmd"},{"id":"1","name":"read_file","arguments":"{\"target_file\":\"src/a.rs\"}"}]}"#,
            r#"{"type":"user","synthetic_reason":"system_reminder","content":[{"type":"text","text":"<system-reminder>x</system-reminder>"}]}"#,
            r#"{"type":"tool_result","content":"ok"}"#,
        ]
        .join("\n");
        let m: Vec<(&str, String)> = parse_grok(&t).into_iter().map(|m| (m.role, m.text)).collect();
        assert_eq!(
            m,
            vec![
                ("user", "ship it".into()),
                ("assistant", "Shipping.".into()),
                ("tool", "run_terminal_cmd".into()),
                ("tool", "read_file · src/a.rs".into()),
            ]
        );
    }

    #[test]
    fn grok_accepts_native_and_openai_calls_and_keeps_tool_output() {
        let input = [
            r#"{"type":"assistant","tool_calls":[{"id":"a","name":"read_file","arguments":{"target_file":"a.rs"}},{"id":"b","function":{"name":"run_terminal_cmd","arguments":"{\"command\":\"cargo test\"}"}}]}"#,
            r#"{"type":"tool","tool_call_id":"b","is_error":true,"content":"exit 101\nassertion failed"}"#,
            r#"{"type":"tool_result","tool_call_id":"a","content":[{"type":"text","text":"fn main() {}"}]}"#,
            "{partial",
        ].join("\n");
        let messages = parse_grok(&input);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].text, "read_file · a.rs");
        assert_eq!(messages[1].text, "run_terminal_cmd · cargo test");
        assert_eq!(messages[2].text, "run_terminal_cmd · failed\nexit 101\nassertion failed");
        assert_eq!(messages[3].text, "read_file · output\nfn main() {}");
    }

    #[test]
    fn long_messages_are_capped_on_a_char_boundary() {
        let s = "é".repeat(MAX_MESSAGE_BYTES);
        let c = cap(&s);
        assert!(c.len() <= MAX_MESSAGE_BYTES + "…".len());
        assert!(c.ends_with('…'));
    }

    #[test]
    fn transcripts_are_found_from_ids_only() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join(".claude/projects/-Users-x")).unwrap();
        std::fs::write(h.join(".claude/projects/-Users-x/abc-1.jsonl"), "").unwrap();
        assert_eq!(claude_path(h, "abc-1"), Some(h.join(".claude/projects/-Users-x/abc-1.jsonl")));
        assert_eq!(claude_path(h, "../../etc/passwd"), None);
        assert_eq!(claude_path(h, "nope"), None);

        let day = h.join(".codex/sessions/2026/10/11");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-10-11T01-02-03-def-2.jsonl"), "").unwrap();
        assert_eq!(codex_path(h, "def-2"), Some(day.join("rollout-2026-10-11T01-02-03-def-2.jsonl")));
        assert_eq!(codex_path(h, "f-2"), None);
        let rollout = day.join("rollout-2026-10-11T01-02-03-def-2.jsonl").to_string_lossy().into_owned();
        assert_eq!(codex_open(h, &["/dev/null".into(), rollout.clone()]), Some(rollout.into()));
        let outside = h.join(".codex/sessions/../x/rollout-1.jsonl").to_string_lossy().into_owned();
        assert_eq!(codex_open(h, &[outside]), None);

        // Without a tag, the newest Claude session of the cwd's directory.
        let proj = h.join(".claude/projects/-Users-x-my-app");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("old-1.jsonl"), "").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(proj.join("new-2.jsonl"), "").unwrap();
        assert_eq!(claude_newest(h, "/Users/x/my_app"), Some(proj.join("new-2.jsonl")));
        assert_eq!(claude_newest(h, "relative"), None);

        // One grok process holds a main session, its subagents' sessions and
        // older sessions open; the most recent main session wins.
        let old = h.join(".grok/sessions/%2Fa/s1");
        let main = h.join(".grok/sessions/%2Fa/s2");
        let sub = h.join(".grok/sessions/%2Fa/s3");
        for s in [&old, &main, &sub] {
            std::fs::create_dir_all(s).unwrap();
            std::fs::write(s.join("chat_history.jsonl"), "").unwrap();
        }
        std::fs::write(sub.join("summary.json"), r#"{"session_kind":"subagent"}"#).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(main.join("chat_history.jsonl"), "{}").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(sub.join("chat_history.jsonl"), "{}").unwrap();
        let ev = |s: &Path| s.join("events.jsonl").to_string_lossy().into_owned();
        let open = vec![ev(&old), ev(&main), ev(&sub), main.join("terminal/x.log").to_string_lossy().into(), "/elsewhere/file".into()];
        assert_eq!(grok_session(h, &open), Some(main.join("chat_history.jsonl")));
        // Only subagent sessions open: the newest of them.
        assert_eq!(grok_session(h, &[ev(&sub)]), Some(sub.join("chat_history.jsonl")));
        assert_eq!(grok_session(h, &[h.join(".grok/sessions/%2Fa/../%2Fa/s2/x").to_string_lossy().into()]), None);
    }

    /// Parses this machine's newest real transcripts when there are any;
    /// prints only counts. `cargo test -p lmux real_transcripts -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_transcripts_parse() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let newest = |pattern: &str| -> Option<PathBuf> {
            let out = std::process::Command::new("sh").env_remove("OPENAI_API_KEY").arg("-c").arg(format!("ls -t {pattern} 2>/dev/null | head -1")).output().ok()?;
            let p = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            (!p.is_empty()).then(|| p.into())
        };
        let h = home.display();
        for (agent, pattern) in [
            ("claude", format!("{h}/.claude/projects/*/*.jsonl")),
            ("codex", format!("{h}/.codex/sessions/*/*/*/rollout-*.jsonl")),
            ("grok", format!("{h}/.grok/sessions/*/*/chat_history.jsonl")),
        ] {
            let Some(p) = newest(&pattern) else { continue };
            let msgs = read_chat(agent, &p).unwrap();
            let count = |r: &str| msgs.iter().filter(|m| m["role"] == r).count();
            println!("{agent}: {} messages (user {}, assistant {}, tool {})", msgs.len(), count("user"), count("assistant"), count("tool"));
            assert!(msgs.len() <= MAX_MESSAGES);
            assert!(msgs.iter().all(|m| m["text"].as_str().is_some_and(|t| !t.is_empty() && t.len() <= MAX_MESSAGE_BYTES + 3)));
            assert!(count("assistant") > 0, "{agent}: no assistant messages");
        }
    }

    #[test]
    fn only_the_tail_is_read_from_a_whole_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let line = r#"{"type":"user","message":{"content":"hi"}}"#;
        let mut body = "x".repeat(TAIL_BYTES as usize);
        body.push('\n');
        body.push_str(line);
        body.push('\n');
        std::fs::write(&p, body).unwrap();
        let msgs = read_chat("claude", &p).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["text"], "hi");
    }
}
