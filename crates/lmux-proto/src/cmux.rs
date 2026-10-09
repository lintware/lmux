//! Importing a cmux session: the workspace spec that `workspace.import` builds
//! in one call, and the reader that turns `cmux restore-session --export` JSON
//! into those specs.
//!
//! The export is cmux's own session file, so the reader is lenient: a field it
//! does not find leaves that part of the workspace at lmux's default rather
//! than failing the import.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SplitDirection;

/// One workspace to build: `workspace.import {workspace: ImportWorkspace}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportWorkspace {
    pub title: String,
    /// A top-level folder by name: the first one with this name, else a new one.
    #[serde(default)]
    pub folder: Option<String>,
    pub layout: ImportLayout,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImportLayout {
    Pane {
        tabs: Vec<ImportTab>,
        /// Index of the selected tab.
        #[serde(default)]
        selected: usize,
        #[serde(default)]
        focused: bool,
    },
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<ImportLayout>,
        second: Box<ImportLayout>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportTab {
    /// "terminal" or "browser".
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// A command to run instead of a shell, e.g. re-attaching the tmux session
    /// the tab was showing, so its processes carry on.
    #[serde(default)]
    pub command: Option<String>,
    /// An agent session that ran in this tab, resumed in it.
    #[serde(default)]
    pub agent: Option<ImportAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportAgent {
    /// "claude" (the only agent lmux resumes today).
    pub agent: String,
    pub id: String,
    /// Flags it was started with, kept on resume (e.g. a permission mode).
    #[serde(default)]
    pub args: Vec<String>,
}

impl ImportAgent {
    /// The shell line that resumes this session, without the newline.
    pub fn resume_command(&self) -> String {
        let mut cmd = format!("claude --resume {}", self.id);
        for a in &self.args {
            cmd.push(' ');
            cmd.push_str(&sh_quote(a));
        }
        cmd
    }
}

impl ImportLayout {
    pub fn tabs(&self) -> Vec<&ImportTab> {
        match self {
            ImportLayout::Pane { tabs, .. } => tabs.iter().collect(),
            ImportLayout::Split { first, second, .. } => {
                let mut v = first.tabs();
                v.extend(second.tabs());
                v
            }
        }
    }
}

fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_=./:@%+,".contains(&b)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// A cmux workspace as read from an export: where it ran and what to build.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CmuxWorkspace {
    /// cmux's id for it, which `cmux close-workspace --workspace` accepts.
    pub cmux_id: String,
    /// The ssh destination of a remote workspace; None runs on this machine.
    pub host: Option<String>,
    pub spec: ImportWorkspace,
}

/// Read every workspace of every window in a `cmux restore-session --export`.
pub fn from_cmux_export(export: &Value) -> Vec<CmuxWorkspace> {
    let mut out = vec![];
    for window in export["windows"].as_array().into_iter().flatten() {
        let tm = &window["tabManager"];
        let groups: Vec<(&str, &str)> = tm["workspaceGroups"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|g| Some((g["anchorWorkspaceId"].as_str()?, g["name"].as_str()?)))
            .collect();
        for ws in tm["workspaces"].as_array().into_iter().flatten() {
            if let Some(w) = workspace(ws, &groups) {
                out.push(w);
            }
        }
    }
    out
}

fn workspace(ws: &Value, groups: &[(&str, &str)]) -> Option<CmuxWorkspace> {
    let cmux_id = ws["workspaceId"].as_str()?.to_string();
    let host = ws["remote"]["destination"].as_str().map(str::to_string);
    let title = ["customTitle", "processTitle"]
        .iter()
        .find_map(|k| ws[k].as_str().filter(|s| !s.is_empty()))
        .unwrap_or("workspace")
        .to_string();
    let folder = groups.iter().find(|(id, _)| *id == cmux_id).map(|(_, n)| n.to_string());
    let panels: Vec<&Value> = ws["panels"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let focused = ws["focusedPanelId"].as_str();
    let ctx = Ctx { panels: &panels, remote: host.is_some(), ws_cwd: ws["currentDirectory"].as_str(), focused };
    let layout = layout(&ws["layout"], &ctx)?;
    Some(CmuxWorkspace { cmux_id, host, spec: ImportWorkspace { title, folder, layout } })
}

struct Ctx<'a> {
    panels: &'a [&'a Value],
    remote: bool,
    ws_cwd: Option<&'a str>,
    focused: Option<&'a str>,
}

fn layout(node: &Value, ctx: &Ctx) -> Option<ImportLayout> {
    match node["type"].as_str()? {
        "pane" => {
            let pane = &node["pane"];
            let ids: Vec<&str> = pane["panelIds"].as_array()?.iter().filter_map(|v| v.as_str()).collect();
            let tabs: Vec<ImportTab> = ids.iter().filter_map(|id| tab(id, ctx)).collect();
            if tabs.is_empty() {
                return None;
            }
            let kept: Vec<&str> = ids.iter().copied().filter(|id| panel(id, ctx).is_some()).collect();
            let selected = pane["selectedPanelId"]
                .as_str()
                .and_then(|s| kept.iter().position(|id| *id == s))
                .unwrap_or(0);
            let focused = ctx.focused.is_some_and(|f| kept.contains(&f));
            Some(ImportLayout::Pane { tabs, selected, focused })
        }
        "split" => {
            let s = &node["split"];
            let first = layout(&s["first"], ctx);
            let second = layout(&s["second"], ctx);
            match (first, second) {
                (Some(a), Some(b)) => Some(ImportLayout::Split {
                    // cmux names a split by the axis its panes run along, as
                    // lmux does: "horizontal" is side by side.
                    direction: if s["orientation"].as_str() == Some("vertical") {
                        SplitDirection::Vertical
                    } else {
                        SplitDirection::Horizontal
                    },
                    ratio: (s["dividerPosition"].as_f64().unwrap_or(0.5) as f32).clamp(0.05, 0.95),
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (a, b) => a.or(b),
            }
        }
        _ => None,
    }
}

fn panel<'a>(id: &str, ctx: &Ctx<'a>) -> Option<&'a Value> {
    ctx.panels.iter().copied().find(|p| p["id"].as_str() == Some(id))
}

fn tab(id: &str, ctx: &Ctx) -> Option<ImportTab> {
    let p = panel(id, ctx)?;
    let title = p["title"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    match p["type"].as_str()? {
        "browser" => Some(ImportTab {
            kind: "browser".into(),
            title,
            cwd: None,
            url: p["browser"]["urlString"].as_str().map(str::to_string),
            command: None,
            agent: None,
        }),
        "terminal" => {
            // A remote workspace's own directory is the laptop's; only a
            // directory the remote shell reported belongs to the host.
            let cwd = if ctx.remote {
                p["directory"].as_str().filter(|_| p["directoryIsTrustedRemoteReport"] == true)
            } else {
                p["directory"].as_str().or(ctx.ws_cwd)
            };
            let binding = &p["terminal"]["resumeBinding"];
            let command = match binding["kind"].as_str() {
                Some("tmux") => {
                    let argv: Vec<&str> = binding["launchCommand"]["arguments"]
                        .as_array()
                        .map(|v| v.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default();
                    // A cmux tab that only views an lmux tab: that tab is in
                    // lmux already.
                    if argv.windows(2).any(|w| w == ["-L", "lmux"]) {
                        return None;
                    }
                    binding["command"].as_str().map(str::to_string)
                }
                _ => None,
            };
            Some(ImportTab {
                kind: "terminal".into(),
                title,
                cwd: cwd.map(str::to_string),
                url: None,
                agent: if command.is_some() { None } else { agent(binding, &p["terminal"]["agent"]) },
                command,
            })
        }
        _ => None,
    }
}

/// The Claude session a tab held. cmux's resume binding is the record it
/// resumes from itself, so it wins over the agent the hooks last reported.
fn agent(binding: &Value, reported: &Value) -> Option<ImportAgent> {
    let (id, launch, mode) = if binding["kind"].as_str() == Some("claude") {
        (binding["checkpointId"].as_str()?, &binding["launchCommand"], binding["permissionMode"].as_str())
    } else if reported["kind"].as_str() == Some("claude") {
        (reported["sessionId"].as_str()?, &reported["launchCommand"], reported["permissionMode"].as_str())
    } else {
        return None;
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    // The flags it was launched with, minus the program and the ones that name
    // a session or point at cmux's own settings.
    let mut args = vec![];
    let launch: Vec<&str> = launch["arguments"]
        .as_array()
        .map(|v| v.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    let mut it = launch.iter().skip(1);
    while let Some(arg) = it.next() {
        let takes_value = ["--resume", "-r", "--session-id", "--settings", "--mcp-config"];
        if takes_value.contains(arg) {
            it.next();
        } else if ["--continue", "-c"].contains(arg) || takes_value.iter().any(|f| arg.starts_with(&format!("{f}="))) {
        } else {
            args.push(arg.to_string());
        }
    }
    // The permission mode it was in last, which may have changed since launch.
    let has_mode = args.iter().any(|a| a == "--dangerously-skip-permissions" || a.starts_with("--permission-mode"));
    match mode {
        _ if has_mode => {}
        Some("bypassPermissions") => args.push("--dangerously-skip-permissions".into()),
        Some(m) if m != "default" && !m.is_empty() => {
            args.push("--permission-mode".into());
            args.push(m.into());
        }
        _ => {}
    }
    Some(ImportAgent { agent: "claude".into(), id: id.to_string(), args })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn export() -> Value {
        json!({"windows": [{"tabManager": {
            "workspaceGroups": [{"anchorWorkspaceId": "W2", "name": "Clients"}],
            "workspaces": [
                {"workspaceId": "W1", "processTitle": "Braid", "currentDirectory": "/p",
                 "focusedPanelId": "b",
                 "layout": {"type": "split", "split": {"orientation": "horizontal", "dividerPosition": 0.66,
                    "first": {"type": "pane", "pane": {"panelIds": ["a", "gone"], "selectedPanelId": "a"}},
                    "second": {"type": "pane", "pane": {"panelIds": ["b", "c", "v", "t"], "selectedPanelId": "c"}}}},
                 "panels": [
                    {"id": "a", "type": "terminal", "title": "Claude", "directory": "/p",
                     "terminal": {"agent": {"kind": "claude", "sessionId": "abc-123",
                        "launchCommand": {"arguments": ["/bin/claude", "--dangerously-skip-permissions",
                            "--settings", "/tmp/x.json", "--model", "opus 5"]}}}},
                    {"id": "b", "type": "browser", "title": "Docs", "browser": {"urlString": "https://x.org/"}},
                    {"id": "c", "type": "terminal", "title": "shell",
                     "terminal": {"resumeBinding": {"kind": "claude", "checkpointId": "def-456",
                        "permissionMode": "auto", "launchCommand": {"arguments": ["/bin/claude"]}}}},
                    {"id": "v", "type": "terminal", "terminal": {"resumeBinding": {"kind": "tmux",
                        "command": "tmux -L lmux attach -t view-1",
                        "launchCommand": {"arguments": ["tmux", "-L", "lmux", "attach", "-t", "view-1"]}}}},
                    {"id": "t", "type": "terminal", "terminal": {"resumeBinding": {"kind": "tmux",
                        "command": "tmux attach -t work",
                        "launchCommand": {"arguments": ["tmux", "attach", "-t", "work"]}}}}]},
                {"workspaceId": "W2", "customTitle": "remote", "currentDirectory": "/Users/me",
                 "remote": {"destination": "box"},
                 "layout": {"type": "pane", "pane": {"panelIds": ["d"]}},
                 "panels": [{"id": "d", "type": "terminal", "directory": "/Users/me",
                             "directoryIsTrustedRemoteReport": false}]}
            ]}}]})
    }

    #[test]
    fn reads_layout_tabs_agents_and_folders() {
        let ws = from_cmux_export(&export());
        assert_eq!(ws.len(), 2);
        let w = &ws[0];
        assert_eq!((w.cmux_id.as_str(), w.host.as_deref(), w.spec.title.as_str()), ("W1", None, "Braid"));
        let ImportLayout::Split { direction, ratio, first, second } = &w.spec.layout else { panic!() };
        assert_eq!(*direction, SplitDirection::Horizontal);
        assert!((ratio - 0.66).abs() < 1e-6);
        // A panel id with no panel is dropped, and selection follows the ids kept.
        let ImportLayout::Pane { tabs, selected, focused } = first.as_ref() else { panic!() };
        assert_eq!((tabs.len(), *selected, *focused), (1, 0, false));
        let a = tabs[0].agent.as_ref().unwrap();
        assert_eq!(a.resume_command(), "claude --resume abc-123 --dangerously-skip-permissions --model 'opus 5'");
        let ImportLayout::Pane { tabs, selected, focused } = second.as_ref() else { panic!() };
        assert_eq!((tabs[0].url.as_deref(), *selected, *focused), (Some("https://x.org/"), 1, true));
        assert_eq!(tabs[1].cwd.as_deref(), Some("/p"));
        // A resume binding names the session and its last permission mode.
        assert_eq!(
            tabs[1].agent.as_ref().unwrap().resume_command(),
            "claude --resume def-456 --permission-mode auto"
        );
        // A view of an lmux tab is dropped; another tmux session is re-attached.
        assert_eq!(tabs.len(), 3);
        assert_eq!((tabs[2].command.as_deref(), tabs[2].agent.is_none()), (Some("tmux attach -t work"), true));

        let r = &ws[1];
        assert_eq!((r.host.as_deref(), r.spec.folder.as_deref()), (Some("box"), Some("Clients")));
        // The laptop's directory is not used on the remote host.
        assert_eq!(r.spec.layout.tabs()[0].cwd, None);
    }
}
