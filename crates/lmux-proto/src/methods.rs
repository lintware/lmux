//! RPC method names. Names mirror cmux CLI commands so agents can reuse habits.
//!
//! Params are JSON objects. Any target param (`workspace`, `pane`, `surface`)
//! may be omitted to mean "the caller's own", resolved from the
//! `LMUX_WORKSPACE_ID` / `LMUX_SURFACE_ID` env vars the CLI sends as
//! `caller_workspace` / `caller_surface` in every request's params.
//!
//! Results are JSON objects; every create returns the new refs, e.g.
//! `{"workspace": "workspace:3", "pane": "pane:5", "surface": "surface:8"}`.

// --- inspect ---
pub const PING: &str = "system.ping"; // {} -> {"pong": true}
pub const IDENTIFY: &str = "system.identify"; // {} -> {workspace, pane, surface}
pub const TREE: &str = "system.tree"; // {} -> Tree

// --- folders & workspaces ---
pub const FOLDER_NEW: &str = "folder.new"; // {name, parent?} -> {folder} (parent nests it inside another folder)
pub const FOLDER_RENAME: &str = "folder.rename"; // {folder, name}
pub const FOLDER_CLOSE: &str = "folder.close"; // {folder} (its workspaces and child folders move up to its parent)
pub const FOLDER_MOVE: &str = "folder.move"; // {folder, parent?: Ref|null, index?} (reparent/reorder; a folder cannot move into itself or a descendant; `parent` null/absent is the top level, `index` counts the parent's child folders)
pub const WORKSPACE_NEW: &str = "workspace.new"; // {name?, cwd?, command?, folder?} -> {workspace, pane, surface}
pub const WORKSPACE_LIST: &str = "workspace.list"; // {} -> {workspaces: [..]}
pub const WORKSPACE_SELECT: &str = "workspace.select"; // {workspace}
pub const WORKSPACE_RENAME: &str = "workspace.rename"; // {workspace?, title}
pub const WORKSPACE_CLOSE: &str = "workspace.close"; // {workspace}
pub const WORKSPACE_MOVE: &str = "workspace.move"; // {workspace, folder?: Ref|null, index?}

// --- panes & surfaces ---
pub const SPLIT_NEW: &str = "pane.split"; // {side: SplitSide, surface?, type?: "terminal"|"browser", url?, command?, cwd?} -> {pane, surface}
pub const PANE_LIST: &str = "pane.list"; // {workspace?} -> {panes: [..]}
pub const PANE_FOCUS: &str = "pane.focus"; // {pane}
pub const PANE_RESIZE: &str = "pane.resize"; // {pane, ratio}
pub const SURFACE_NEW: &str = "surface.new"; // {pane?, type?, url?, command?, cwd?} -> {surface} (new tab in pane)
pub const SURFACE_CLOSE: &str = "surface.close"; // {surface?}
pub const SURFACE_RENAME: &str = "surface.rename"; // {surface?, title}
// Drag-and-drop rearranging in the client. Together these cover dragging a tab
// within its strip, onto another pane, and onto a pane's edge to make a split.
pub const SURFACE_MOVE: &str = "surface.move"; // {surface, pane, index?} -> {pane} (move a tab to another pane, or reorder it within its own; a pane left empty is removed and its split collapses; `index` defaults to the end)
pub const SURFACE_SPLIT: &str = "surface.split"; // {surface, side: SplitSide, pane?} -> {pane} (move an existing tab into a new pane split off `pane`, or off the tab's own pane when `pane` is absent, on `side`; a source pane left empty collapses)
pub const PANE_SWAP: &str = "pane.swap"; // {pane, other} -> {} (exchange two panes' positions in the workspace layout)

// --- terminal control ---
pub const TERM_SEND: &str = "term.send"; // {surface?, text} (text is sent literally; "\n" is Enter)
pub const TERM_SEND_KEY: &str = "term.send_key"; // {surface?, key} e.g. "enter", "ctrl+c", "escape", "up"
pub const TERM_READ_SCREEN: &str = "term.read_screen"; // {surface?, scrollback?: bool, lines?: u32} -> {text}
pub const TERM_CLEAR_HISTORY: &str = "term.clear_history"; // {surface?}

// --- agent awareness ---
pub const NOTIFY: &str = "notify.create"; // {title, subtitle?, body?, workspace?, surface?} -> {id}
pub const NOTIFY_LIST: &str = "notify.list"; // {} -> {notifications: [..]}
pub const NOTIFY_CLEAR: &str = "notify.clear"; // {id? | workspace?}
pub const NOTIFY_MARK_READ: &str = "notify.mark_read"; // {id? | workspace?}
pub const STATUS_SET: &str = "status.set"; // {key, value, icon?, color?, workspace?}
pub const STATUS_CLEAR: &str = "status.clear"; // {key, workspace?}
pub const PROGRESS_SET: &str = "progress.set"; // {value, label?, workspace?}
pub const PROGRESS_CLEAR: &str = "progress.clear"; // {workspace?}
pub const LOG: &str = "log.append"; // {message, level?, source?, workspace?}
pub const LOG_LIST: &str = "log.list"; // {workspace?, limit?} -> {entries: [..]}
pub const AGENT_STATE: &str = "agent.state"; // {state: AgentState, summary?, workspace?} (called by Claude Code hooks)
pub const AGENT_SESSION: &str = "agent.session"; // {surface, agent, id, cwd?, source?} (SessionStart hook; resumed after a tmux restart)

// --- browser (implemented by lmux-browser, routed by the daemon) ---
pub const BROWSER_OPEN: &str = "browser.open"; // {url?, side?: SplitSide, surface?} -> {surface}
pub const BROWSER_GOTO: &str = "browser.goto"; // {surface?, url}
pub const BROWSER_BACK: &str = "browser.back"; // {surface?}
pub const BROWSER_FORWARD: &str = "browser.forward"; // {surface?}
pub const BROWSER_RELOAD: &str = "browser.reload"; // {surface?}
pub const BROWSER_URL: &str = "browser.url"; // {surface?} -> {url, title}
pub const BROWSER_SNAPSHOT: &str = "browser.snapshot"; // {surface?, interactive?: bool} -> {text, refs}
pub const BROWSER_CLICK: &str = "browser.click"; // {surface?, target} target = "e12" ref or CSS selector
pub const BROWSER_DBLCLICK: &str = "browser.dblclick"; // {surface?, target}
pub const BROWSER_HOVER: &str = "browser.hover"; // {surface?, target}
pub const BROWSER_TYPE: &str = "browser.type"; // {surface?, target, text}
pub const BROWSER_FILL: &str = "browser.fill"; // {surface?, target, text}
pub const BROWSER_PRESS: &str = "browser.press"; // {surface?, key}
pub const BROWSER_SELECT: &str = "browser.select"; // {surface?, target, value}
pub const BROWSER_SCROLL: &str = "browser.scroll"; // {surface?, target?, dx?, dy?}
pub const BROWSER_WAIT: &str = "browser.wait"; // {surface?, selector?, text?, url_contains?, load_state?, timeout_ms?}
pub const BROWSER_EVAL: &str = "browser.eval"; // {surface?, script} -> {value}
pub const BROWSER_SCREENSHOT: &str = "browser.screenshot"; // {surface?, path?} -> {path} | {png_base64}
pub const BROWSER_GET: &str = "browser.get"; // {surface?, what: "url"|"title"|"text"|"html"|"value"|"attr"|"count", target?, name?} -> {value}
pub const BROWSER_CONSOLE: &str = "browser.console"; // {surface?, clear?: bool} -> {entries}
pub const BROWSER_ERRORS: &str = "browser.errors"; // {surface?, clear?: bool} -> {entries}
pub const BROWSER_TABS: &str = "browser.tabs"; // {surface?, action: "list"|"new"|"switch"|"close", index?, url?}
