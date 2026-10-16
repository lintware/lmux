//! Command-line definitions. Names and flags follow cmux's CLI so agents can
//! reuse their cmux habits with `cmux` replaced by `lmux`; lmux's own older
//! spellings stay as aliases.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The top-level help: commands grouped the way cmux groups them. A test keeps
/// it in step with the subcommands.
pub const COMMANDS_HELP: &str = "\
Workspaces:
  new-workspace              Create a workspace (--name, --cwd, --command, --folder; a folder's cwd is the default)
  list-workspaces            List workspaces
  current-workspace          Print the caller's (else the selected) workspace
  select-workspace           Select a workspace
  rename-workspace           Rename a workspace
  reorder-workspace          Move a workspace (--index, --before, --after, --folder)
  close-workspace            Close a workspace and its terminals
  new-folder                 Create a sidebar folder (--parent, --cwd: where its sessions start)
  set-folder-cwd             Set or clear (--clear) a folder's working directory
  move-folder                Move a folder into another one, or to the top level
  close-folder               Close a folder; its workspaces and folders move up (--if-empty)
  tree                       Print workspaces, panes and surfaces
  identify                   Print the caller's workspace, pane and surface

Panes and surfaces (tabs):
  new-split                  Split a pane: new-split <left|right|up|down>
  new-pane                   Add a pane (--type terminal|browser, --direction)
  new-surface                Add a tab to a pane
  list-panes                 List a workspace's panes
  list-pane-surfaces         List a pane's tabs
  focus-pane                 Focus a pane
  resize-pane                Set a pane's split ratio
  swap-pane                  Exchange two panes
  close-surface              Close a tab
  rename-tab                 Rename a tab
  move-surface               Move a tab to another pane or workspace
  reorder-surface            Reorder a tab within its pane
  split-off                  Move a tab into a new split
  move-tab-to-new-workspace  Move a tab into a new workspace

Terminal:
  send                       Type text (\\n is Enter, \\t is Tab)
  send-key                   Press a key (enter, ctrl+c, up, ...)
  paste                      Send text as is, from the argument or stdin
  harness-status             Query per-surface lifecycle, queue and delivery evidence
  harness-config             Query or request per-surface model and effort changes
  read-screen                Print a terminal's screen (--scrollback, --lines)
  capture-pane               tmux-style read-screen (-p, -S, -E, -J, -e)
  wait-for                   Wait for text on screen (--text), or a named signal
  clear-history              Clear a terminal's scrollback

Notifications:
  notify                     Post a notification
  list-notifications         List notifications
  jump-to-unread             Select the newest unread notification's workspace
  open-notification          Select a notification's workspace and tab
  mark-notification-read     Mark notifications read
  dismiss-notification       Remove one notification, or every read one
  clear-notifications        Remove notifications (the caller's workspace, else all)

Sidebar status:
  set-status                 Set a status pill
  clear-status               Remove a status pill
  list-status                List status pills
  set-progress               Set the progress bar (0.0-1.0)
  clear-progress             Remove the progress bar
  log                        Append a log entry
  list-log                   List log entries
  clear-log                  Clear the log

Browser:
  browser                    Drive a browser pane: lmux browser --help

Other:
  cp                         Copy files or folders to a host: cp FILE... HOST[:WORKSPACE|/PATH] (--tell)
  import-cmux                Bring cmux's workspaces over (--move hands over their Claude sessions)
  share                      Share sessions to a phone or browser through a Cloudflare Quick Tunnel
  share-connect              Register this machine with a phone gateway over outbound SSH
  share-sessions             List the phones paired with the gateway service here
  share-revoke               End a paired phone's session now (ID, or --all)
  rpc                        Call a daemon method: rpc <method> [json-params]
  list-hosts                 Discover local and SSH/Tailscale host routes
  ping                       Check the daemon answers
  hooks                      Claude Code and Grok Build observation hooks
  server                     Run the daemon in the foreground
  help                       Print help for a command

Targets: --workspace/--surface/--pane take refs such as workspace:2, surface:5,
pane:3, or just the number (2). A workspace or folder can also be named by its title.
Left out, they mean the caller's own (from $LMUX_SURFACE_ID and
$LMUX_WORKSPACE_ID), else what is focused. Refs belong to one host.
Use --host HOST to query another daemon, or tree --all-hosts to discover sessions.";

#[derive(Parser, Debug)]
#[command(
    name = "lmux",
    version = lmux_core::VERSION,
    about = "lmux: persistent terminal and browser workspaces, driven like cmux",
    help_template = "{about}\n\n{usage-heading} {usage}{after-help}\n\nOptions:\n{options}\n",
    after_help = COMMANDS_HELP,
)]
pub struct Cli {
    /// Path to the daemon unix socket (default $LMUX_SOCKET or ~/.local/state/lmux/lmux.sock).
    #[arg(long, global = true)]
    pub socket: Option<String>,
    /// Select an SSH/Tailscale host (local keeps the local daemon). An explicit --socket is on that host.
    #[arg(long, global = true)]
    pub host: Option<String>,
    /// Print raw JSON results.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

impl Cli {
    pub fn validate_host_scope(&self) -> anyhow::Result<()> {
        if let Cmd::Tree { all_hosts: true, workspace } = &self.cmd {
            if self.host.is_some() || self.socket.is_some() || workspace.is_some() {
                anyhow::bail!("tree --all-hosts requires an unfiltered host query; select --host, --socket or --workspace in a separate call");
            }
        }
        Ok(())
    }
}

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    #[arg(long, default_value = "lmux")]
    pub tmux_socket: String,
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:7878")]
    pub ws_addr: String,
    /// Run terminals only; every `browser.*` answers Unsupported.
    #[arg(long)]
    pub no_browser: bool,
    /// Chromium binary (default /usr/bin/chromium).
    #[arg(long)]
    pub chromium_path: Option<PathBuf>,
    /// Browser panes are native WebKit views in the lmux app instead of a
    /// Chromium here (implies --no-browser).
    #[arg(long)]
    pub app_browser: bool,
    /// The WebKit helper that runs browser pages here (default
    /// ~/.local/libexec/lmux/lmux-webkit when it exists; else Chromium).
    #[arg(long)]
    pub webkit_helper: Option<PathBuf>,
    /// Use Chromium even when a WebKit helper is installed.
    #[arg(long)]
    pub chromium: bool,
}

/// `--surface` / `--workspace`, as most terminal commands take them.
#[derive(Args, Debug, Clone, Default)]
pub struct Target {
    /// Target surface (default: $LMUX_SURFACE_ID, else the focused one).
    #[arg(long, alias = "panel", value_name = "SURFACE")]
    pub surface: Option<String>,
    /// Workspace context: without --surface, its focused surface.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,
}

/// A `--focus <true|false>` value, as cmux takes it.
fn boolish() -> clap::builder::BoolishValueParser {
    clap::builder::BoolishValueParser::new()
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Run the daemon in the foreground.
    Server(ServerArgs),

    // ----- workspaces ------------------------------------------------------
    /// Create a sidebar folder. With --host it lives on that host, and so do
    /// the workspaces opened in it.
    NewFolder {
        name: String,
        /// Nest the folder inside this one, instead of the top level.
        #[arg(long)]
        parent: Option<String>,
        /// Project directory (on the folder's host) new workspaces in it start in.
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Set a folder's working directory, or clear it with --clear.
    SetFolderCwd {
        /// Folder ref (folder:3), number or title.
        folder: String,
        /// Directory on the folder's host.
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        cwd: Option<String>,
        /// Forget the directory: new workspaces start in the default again.
        #[arg(long)]
        clear: bool,
    },
    /// Close a folder. Its workspaces and child folders move up to its parent.
    CloseFolder {
        /// Folder ref (folder:3), number or title.
        folder: String,
        /// Refuse when the folder still holds workspaces or folders.
        #[arg(long)]
        if_empty: bool,
    },
    /// Move a folder into another one, or to the top level.
    MoveFolder {
        folder: Option<String>,
        /// Put the folder inside this one; omit to move it to the top level.
        #[arg(long)]
        parent: Option<String>,
        /// Slot among that parent's folders (default: last).
        #[arg(long)]
        index: Option<u64>,
    },
    /// Create a workspace with one terminal.
    NewWorkspace {
        /// Title (default: the command's name, else "workspace N").
        #[arg(long)]
        name: Option<String>,
        /// Working directory.
        #[arg(long)]
        cwd: Option<String>,
        /// Command to run instead of a shell.
        #[arg(long)]
        command: Option<String>,
        /// Folder to put it in.
        #[arg(long, alias = "group")]
        folder: Option<String>,
        /// Select the new workspace (default: false).
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
    },
    /// Rename a workspace: rename-workspace [--workspace W] [--] <title>
    #[command(alias = "rename-window")]
    RenameWorkspace {
        /// The new title (every word, joined).
        #[arg(required = true, num_args = 1..)]
        title: Vec<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Close a workspace and its terminals.
    CloseWorkspace {
        #[arg(value_name = "WORKSPACE")]
        target: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        /// Accepted for cmux compatibility; lmux closes without asking.
        #[arg(long)]
        force: bool,
    },
    /// Select a workspace (reads its notifications).
    SelectWorkspace {
        #[arg(value_name = "WORKSPACE")]
        target: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Move a workspace in the sidebar.
    ReorderWorkspace {
        #[arg(value_name = "WORKSPACE")]
        target: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        /// Put it in this folder (with --index; default: the top level).
        #[arg(long)]
        folder: Option<String>,
        /// Slot in the workspace list.
        #[arg(long)]
        index: Option<u64>,
        /// Place it before this workspace (joining its folder).
        #[arg(long, alias = "before-workspace")]
        before: Option<String>,
        /// Place it after this workspace (joining its folder).
        #[arg(long, alias = "after-workspace")]
        after: Option<String>,
    },
    /// List workspaces.
    ListWorkspaces,
    /// Print the caller's workspace, else the selected one.
    CurrentWorkspace,
    /// Print workspaces, panes and surfaces.
    Tree {
        /// Query discovered hosts, grouped by host; report unreachable hosts explicitly.
        #[arg(long, conflicts_with_all = ["host", "workspace", "socket"])]
        all_hosts: bool,
        /// Show only this workspace.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Print the caller's workspace, pane and surface.
    Identify {
        #[command(flatten)]
        target: Target,
    },

    // ----- panes & surfaces --------------------------------------------------
    /// Split a pane: new-split <left|right|up|down>
    NewSplit {
        #[arg(value_parser = ["left", "right", "up", "down", "l", "r", "u", "d"])]
        side: String,
        #[command(flatten)]
        target: Target,
        /// terminal or browser.
        #[arg(long = "type", default_value = "terminal")]
        kind: String,
        /// URL for a browser pane.
        #[arg(long)]
        url: Option<String>,
        /// Command to run instead of a shell.
        #[arg(long)]
        command: Option<String>,
        #[arg(long, alias = "working-directory")]
        cwd: Option<String>,
        /// Focus the new pane (default: true).
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
    },
    /// Add a pane next to the focused one.
    NewPane {
        /// terminal or browser.
        #[arg(long = "type", default_value = "terminal")]
        kind: String,
        /// left, right, up or down (default: right).
        #[arg(long, alias = "side", value_parser = ["left", "right", "up", "down", "l", "r", "u", "d"])]
        direction: Option<String>,
        /// URL for a browser pane.
        #[arg(long)]
        url: Option<String>,
        /// Command to run instead of a shell.
        #[arg(long)]
        command: Option<String>,
        #[arg(long, alias = "working-directory")]
        cwd: Option<String>,
        #[command(flatten)]
        target: Target,
        /// Focus the new pane (default: true).
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
    },
    /// Add a tab to a pane.
    NewSurface {
        /// Pane to add it to (default: the caller's).
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        /// terminal or browser.
        #[arg(long = "type", default_value = "terminal")]
        kind: String,
        /// URL for a browser tab.
        #[arg(long)]
        url: Option<String>,
        /// Command to run instead of a shell.
        #[arg(long)]
        command: Option<String>,
        #[arg(long, alias = "working-directory")]
        cwd: Option<String>,
        /// Focus the new tab (default: true).
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
    },
    /// Close a tab (default: the caller's).
    CloseSurface {
        #[command(flatten)]
        target: Target,
        /// Accepted for cmux compatibility; lmux closes without asking.
        #[arg(long)]
        force: bool,
    },
    /// Rename a tab: rename-tab [--surface S] [--] <title>
    #[command(alias = "rename-surface")]
    RenameTab {
        /// The new title (every word, joined); or use --title.
        #[arg(num_args = 0..)]
        words: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, alias = "panel", alias = "tab")]
        surface: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Move a tab to another pane or workspace, or reorder it.
    MoveSurface {
        #[arg(value_name = "SURFACE")]
        target: Option<String>,
        #[arg(long, alias = "panel")]
        surface: Option<String>,
        /// Destination pane.
        #[arg(long)]
        pane: Option<String>,
        /// Destination workspace (its focused pane).
        #[arg(long)]
        workspace: Option<String>,
        /// Slot in the destination pane (default: last).
        #[arg(long)]
        index: Option<u64>,
        /// Place it before this tab.
        #[arg(long, alias = "before-surface")]
        before: Option<String>,
        /// Place it after this tab.
        #[arg(long, alias = "after-surface")]
        after: Option<String>,
        /// Accepted for cmux compatibility; a moved tab is always focused.
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
    },
    /// Reorder a tab within its pane.
    ReorderSurface {
        #[arg(value_name = "SURFACE")]
        target: Option<String>,
        #[arg(long, alias = "panel")]
        surface: Option<String>,
        #[arg(long)]
        index: Option<u64>,
        #[arg(long, alias = "before-surface")]
        before: Option<String>,
        #[arg(long, alias = "after-surface")]
        after: Option<String>,
    },
    /// Move a tab into a new split: split-off --surface S <left|right|up|down>
    #[command(name = "split-off", alias = "split-surface", alias = "drag-surface-to-split")]
    SplitOff {
        /// `[SURFACE] SIDE`: the tab (unless --surface) and the side.
        #[arg(num_args = 0..=2, value_name = "ARGS")]
        args: Vec<String>,
        #[arg(long, alias = "panel")]
        surface: Option<String>,
        #[arg(long, alias = "direction", value_parser = ["left", "right", "up", "down", "l", "r", "u", "d"])]
        side: Option<String>,
        /// The pane the split comes off (default: the tab's own).
        #[arg(long)]
        pane: Option<String>,
        /// Accepted for cmux compatibility.
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
    },
    /// Move a tab into a new workspace of its own.
    #[command(alias = "detach-tab")]
    MoveTabToNewWorkspace {
        #[arg(long, alias = "panel", alias = "tab")]
        surface: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        /// Title for the new workspace (default: the tab's).
        #[arg(long)]
        title: Option<String>,
        /// Select the new workspace (default: false).
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
    },
    /// Exchange two panes: swap-pane --pane A --target-pane B
    SwapPane {
        #[arg(num_args = 0..=2, value_name = "PANE")]
        panes: Vec<String>,
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        target_pane: Option<String>,
        /// Accepted for cmux compatibility.
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
    },
    /// Focus a pane.
    FocusPane {
        #[arg(value_name = "PANE")]
        target: Option<String>,
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Set the ratio of the split holding a pane.
    ResizePane {
        #[arg(value_name = "PANE")]
        target: Option<String>,
        #[arg(long)]
        pane: Option<String>,
        /// The split's ratio: the first (left or top) side's share, 0.05-0.95.
        #[arg(long)]
        ratio: f32,
    },
    /// List a workspace's panes.
    ListPanes {
        #[arg(long)]
        workspace: Option<String>,
    },
    /// List a pane's tabs.
    ListPaneSurfaces {
        /// Pane (default: the caller's, else the focused one).
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },

    // ----- terminal --------------------------------------------------------
    /// Type text into a terminal. \n and \r press Enter, \t presses Tab.
    Send {
        #[arg(num_args = 0.., allow_hyphen_values = true)]
        text: Vec<String>,
        #[command(flatten)]
        target: Target,
        /// Press Enter after the text.
        #[arg(long)]
        enter: bool,
        /// Send the text as is, like `paste`.
        #[arg(long)]
        paste: bool,
        /// Accepted for cmux compatibility.
        #[arg(long)]
        force: bool,
    },
    /// Press a key: enter, tab, escape, up, ctrl+c, ...
    SendKey {
        key: String,
        #[command(flatten)]
        target: Target,
        /// Accepted for cmux compatibility.
        #[arg(long)]
        force: bool,
    },
    /// Send text as is (no escapes); with no text, or `-`, read stdin.
    Paste {
        #[arg(num_args = 0.., allow_hyphen_values = true)]
        text: Vec<String>,
        #[command(flatten)]
        target: Target,
        /// Press Enter after the paste.
        #[arg(long)]
        submit: bool,
        /// Accepted for cmux compatibility.
        #[arg(long)]
        force: bool,
    },
    #[command(flatten)]
    Harness(HarnessCmd),
    /// Print a terminal's screen as plain text.
    ReadScreen {
        #[command(flatten)]
        target: Target,
        /// Include scrollback, not just the visible screen.
        #[arg(long)]
        scrollback: bool,
        /// Only the last N lines (implies --scrollback).
        #[arg(long)]
        lines: Option<u32>,
    },
    /// tmux-style read-screen: capture-pane [-p] [-S start] [-E end] [-J] [-e]
    CapturePane {
        #[command(flatten)]
        target: Target,
        /// The target surface, as tmux's -t.
        #[arg(short = 't', value_name = "SURFACE")]
        tmux_target: Option<String>,
        /// Print to stdout (always on; accepted for tmux compatibility).
        #[arg(short = 'p')]
        print: bool,
        /// First line: 0 is the top of the screen, negative reaches into history, - is the start of history.
        #[arg(short = 'S', allow_hyphen_values = true, value_name = "START")]
        start: Option<String>,
        /// Last line, as -S; - is the end of the screen.
        #[arg(short = 'E', allow_hyphen_values = true, value_name = "END")]
        end: Option<String>,
        /// Join wrapped lines.
        #[arg(short = 'J')]
        join: bool,
        /// Keep colour escapes.
        #[arg(short = 'e')]
        escape: bool,
        /// Include scrollback (as read-screen).
        #[arg(long)]
        scrollback: bool,
        /// Only the last N lines (implies --scrollback).
        #[arg(long)]
        lines: Option<u32>,
    },
    /// Wait for text on a terminal's screen (--text), or for a named signal.
    ///
    /// `wait-for --text PATTERN` polls the target terminal's screen until
    /// PATTERN appears. `wait-for NAME` blocks until `wait-for -S NAME` is
    /// run; a signal sent first is kept until a waiter takes it.
    WaitFor {
        /// The signal name (not with --text).
        name: Option<String>,
        /// Send the signal instead of waiting for it.
        #[arg(short = 'S', long)]
        signal: bool,
        /// Wait until this text is on the target's screen.
        #[arg(long, value_name = "PATTERN")]
        text: Option<String>,
        /// Search the scrollback too (with --text).
        #[arg(long)]
        scrollback: bool,
        #[command(flatten)]
        target: Target,
        /// Seconds to wait before failing.
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
    /// Clear a terminal's scrollback.
    ClearHistory {
        #[command(flatten)]
        target: Target,
        /// The target surface, as tmux's -t.
        #[arg(short = 't', value_name = "SURFACE")]
        tmux_target: Option<String>,
    },

    // ----- notifications ---------------------------------------------------
    /// Post a notification: notify --title T [--subtitle S] [--body B]
    Notify {
        /// Title (or use --title).
        #[arg(value_name = "TITLE")]
        title_arg: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        subtitle: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        surface: Option<String>,
        /// Clear the caller's (or the target's) notifications instead.
        #[arg(long)]
        clear: bool,
    },
    /// List notifications.
    ListNotifications,
    /// Remove notifications: the given workspace's, else the caller's, else all.
    ClearNotifications {
        #[arg(long)]
        workspace: Option<String>,
        /// Only this tab's (with a workspace).
        #[arg(long)]
        surface: Option<String>,
    },
    /// Remove one notification (--id), or every read one (--all-read).
    DismissNotification {
        #[arg(value_name = "ID")]
        target: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        all_read: bool,
    },
    /// Mark notifications read: --id, --workspace [--surface], or --all.
    MarkNotificationRead {
        #[arg(value_name = "ID")]
        target: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        surface: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Select a notification's workspace and tab, and mark it read.
    OpenNotification {
        #[arg(value_name = "ID")]
        target: Option<String>,
        #[arg(long)]
        id: Option<String>,
    },
    /// Select the newest unread notification's workspace and tab.
    JumpToUnread,

    // ----- sidebar status --------------------------------------------------
    /// Set a status pill: set-status <key> <value>
    SetStatus {
        key: String,
        value: String,
        #[arg(long)]
        icon: Option<String>,
        #[arg(long)]
        color: Option<String>,
        /// Accepted for cmux compatibility; lmux keeps pills in set order.
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Remove a status pill.
    ClearStatus {
        key: String,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// List a workspace's status pills.
    ListStatus {
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Set the progress bar: set-progress <0.0-1.0>
    SetProgress {
        value: f32,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Remove the progress bar.
    ClearProgress {
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Append a log entry: log [--level L] [--source S] [--] <message>
    Log {
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
        /// info, progress, success, warning or error.
        #[arg(long, default_value = "info")]
        level: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// List log entries.
    ListLog {
        #[arg(long)]
        workspace: Option<String>,
        /// Only the last N entries.
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Clear the log (entries are not kept per workspace, so all of it).
    ClearLog {
        #[arg(long)]
        workspace: Option<String>,
    },

    // ----- browser -----------------------------------------------------------
    /// Drive a browser pane: browser [<surface>|--surface S|--workspace W] <subcommand>
    #[command(after_help = BROWSER_HELP)]
    Browser {
        #[command(subcommand)]
        cmd: BrowserCmd,
    },

    // ----- other -------------------------------------------------------------
    /// Copy files or folders to a host, into a workspace's working directory
    /// (HOST:WORKSPACE, a title, fragment or ref), a path (HOST:/dir) or the
    /// home directory (HOST). Prints where each item landed. With --tell, the
    /// paths are also pasted into that workspace's terminal, so the chat
    /// running there can read them. Uses scp over your ssh config.
    Cp {
        /// Files or folders on this machine.
        #[arg(required = true, num_args = 1.., value_name = "SOURCE")]
        sources: Vec<PathBuf>,
        /// HOST[:WORKSPACE|/PATH]; HOST is a name from hosts.json or an ssh target.
        #[arg(value_name = "DEST")]
        dest: String,
        /// Paste the remote paths into the workspace's terminal (no Enter).
        #[arg(long)]
        tell: bool,
    },
    /// Rebuild cmux's workspaces in lmux: folders, splits, tabs, directories,
    /// browser URLs and Claude sessions. With --move, each workspace is closed
    /// in cmux and its Claude resumed in lmux; a Claude mid-turn is left alone.
    /// Remote cmux workspaces are built by lmux on their own host.
    ImportCmux {
        /// Close each workspace in cmux and resume its Claude sessions here
        /// (default: build a copy and leave cmux as it is).
        #[arg(long = "move")]
        move_: bool,
        /// Also move workspaces whose Claude is working.
        #[arg(long)]
        force: bool,
        /// Print what would happen without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Only this cmux workspace (its title or id); repeatable.
        #[arg(long = "workspace")]
        only: Vec<String>,
        /// Read this `cmux restore-session --export` file instead of asking cmux.
        #[arg(long)]
        from: Option<String>,
    },
    /// Call a daemon method and print its JSON result: rpc <method> [json-params]
    Rpc {
        /// e.g. system.tree, term.read_screen
        method: String,
        /// A JSON object (default {}).
        params: Option<String>,
    },
    /// Discover host routes from app hosts.json and literal SSH aliases.
    #[command(alias = "hosts")]
    ListHosts,
    /// Check the daemon answers.
    Ping,
    /// Claude Code hooks.
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },
    /// Show a QR code that opens your sessions in a phone browser through a
    /// Cloudflare Quick Tunnel. View only unless --allow-typing.
    Share {
        /// Let the viewer type into terminals (view only by default).
        #[arg(long)]
        allow_typing: bool,
        /// Serve on this local port (default: any free port).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Skip the tunnel and serve on 127.0.0.1 only (for testing).
        #[arg(long)]
        no_tunnel: bool,
        /// The cloudflared binary (default: found on PATH, or downloaded once).
        #[arg(long)]
        cloudflared: Option<String>,
        /// Also control the sessions on this server, reached over SSH (an alias
        /// from ~/.ssh/config or user@host). Repeat for more servers. Each
        /// server needs lmux installed (deploy/install-remote.sh).
        #[arg(long = "server", value_name = "SSH_TARGET")]
        servers: Vec<String>,
        /// Run as the always-on gateway service: hand out codes through
        /// `lmux share-code` and exit when the tunnel stops.
        #[arg(long)]
        service: bool,
    },
    /// List the phones paired with the gateway service on this machine.
    ShareSessions,
    /// End a paired phone's session at once (the gateway service on this
    /// machine). Its open connections close within a second.
    ShareRevoke {
        /// Session id from `lmux share-sessions`.
        #[arg(conflicts_with = "all", required_unless_present = "all")]
        id: Option<String>,
        /// End every paired phone's session.
        #[arg(long)]
        all: bool,
    },
    /// Print a new pairing code (JSON with the link and QR) from the gateway
    /// service on this machine, or with --status, whether a code is unused.
    #[command(hide = true)]
    ShareCode {
        #[arg(long)]
        status: Option<String>,
    },
    /// Serve this machine's sessions to a share gateway over stdin/stdout
    /// (run by `lmux share --server` through SSH).
    #[command(hide = true)]
    ShareBackend,
    /// Register this machine with an always-on gateway over outbound SSH.
    ShareConnect { gateway: String },
    #[command(hide = true)]
    ShareAttach { #[arg(long)] name: String },
    /// Diagnostic: connect to the websocket, subscribe, print snapshot/output.
    #[command(hide = true)]
    WsProbe {
        #[arg(long)]
        surface: String,
        #[arg(long, default_value = "127.0.0.1:7878")]
        addr: String,
        #[arg(long, default_value_t = 6)]
        timeout: u64,
        /// Send this many term_resize messages; the probe reports how many Tree
        /// messages it received, so coalescing is visible.
        #[arg(long, default_value_t = 1)]
        resizes: u64,
    },
}

pub const BROWSER_HELP: &str = "\
The surface is --surface S, or the first word after `browser`:
  lmux browser surface:4 snapshot -i
Without one, commands act on a browser in the caller's workspace.
`open` with a surface navigates it, as in cmux; `open-split` always splits.
Outside any workspace, `open` makes a new workspace for the browser.";

// `--surface` for browser commands. A plain comment: clap would show a doc
// comment as the help line of every subcommand without its own.
#[derive(Args, Debug, Clone, Default)]
pub struct BrowserTarget {
    /// Browser surface (default: a browser in the caller's workspace).
    #[arg(long)]
    pub surface: Option<String>,
    /// Without --surface, a browser in this workspace instead of the caller's.
    #[arg(long, value_name = "WORKSPACE")]
    pub workspace: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum BrowserCmd {
    /// Open a browser: in a new split, or (given a surface) navigate it.
    Open {
        #[arg(num_args = 0..)]
        url: Vec<String>,
        #[arg(long, value_parser = ["left", "right", "up", "down"])]
        side: Option<String>,
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Open a browser in a new split.
    #[command(alias = "new")]
    OpenSplit {
        #[arg(num_args = 0..)]
        url: Vec<String>,
        #[arg(long, value_parser = ["left", "right", "up", "down"])]
        side: Option<String>,
        #[arg(long, value_parser = boolish())]
        focus: Option<bool>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Load a URL.
    #[command(alias = "navigate")]
    Goto {
        #[arg(required = true, num_args = 1..)]
        url: Vec<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Go back in history.
    Back {
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Go forward in history.
    Forward {
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Reload the page.
    Reload {
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Print the URL and title.
    #[command(alias = "get-url")]
    Url {
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Accessibility tree with element refs.
    Snapshot {
        #[arg(short, long)]
        interactive: bool,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Click an element.
    Click {
        /// Element ref (e12) or CSS selector.
        #[arg(value_name = "TARGET")]
        selector_arg: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Double-click an element.
    Dblclick {
        #[arg(value_name = "TARGET")]
        selector_arg: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Move the mouse over an element.
    Hover {
        #[arg(value_name = "TARGET")]
        selector_arg: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Type into an element: type <target> <text>
    Type {
        #[arg(num_args = 0..)]
        args: Vec<String>,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Replace an input's value: fill <target> <text>
    Fill {
        #[arg(num_args = 0..)]
        args: Vec<String>,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Press a key.
    #[command(alias = "key")]
    Press {
        #[arg(value_name = "KEY")]
        key_arg: Option<String>,
        #[arg(long)]
        key: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Real mouse input at page coordinates (CSS pixels from the top left),
    /// which reaches cross-origin iframes: mouse click 120 340
    Mouse {
        /// click, dblclick, down, up, move or wheel
        #[arg(value_name = "ACTION")]
        action: String,
        #[arg(value_name = "X", allow_hyphen_values = true)]
        x: f64,
        #[arg(value_name = "Y", allow_hyphen_values = true)]
        y: f64,
        /// left, right or middle
        #[arg(long)]
        button: Option<String>,
        /// Wheel distance, for `wheel`.
        #[arg(long, allow_hyphen_values = true)]
        dx: Option<f64>,
        #[arg(long, allow_hyphen_values = true)]
        dy: Option<f64>,
        #[arg(long)]
        shift: bool,
        #[arg(long)]
        ctrl: bool,
        #[arg(long)]
        alt: bool,
        #[arg(long)]
        meta: bool,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Real key events for whatever has focus, inside cross-origin iframes
    /// too: keyboard --text "hello", keyboard --key ctrl+a
    Keyboard {
        /// Text to type.
        #[arg(long)]
        text: Option<String>,
        /// A key or combination to press: Enter, Tab, ctrl+a.
        #[arg(long)]
        key: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Choose an option: select <target> <value>
    Select {
        #[arg(num_args = 0..)]
        args: Vec<String>,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        value: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Scroll the page, or an element into view: --dy 300, --selector S.
    Scroll {
        #[arg(value_name = "TARGET")]
        selector_arg: Option<String>,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        dx: Option<f64>,
        #[arg(long, allow_hyphen_values = true)]
        dy: Option<f64>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Wait for a selector, text, URL or load state.
    Wait {
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long, alias = "url")]
        url_contains: Option<String>,
        #[arg(long)]
        load_state: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// Seconds (cmux); --timeout-ms wins.
        #[arg(long)]
        timeout: Option<f64>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Run JavaScript and print its value.
    Eval {
        #[arg(num_args = 0..)]
        script_args: Vec<String>,
        #[arg(long)]
        script: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// Save a PNG of the page.
    Screenshot {
        #[arg(long, alias = "out")]
        path: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// get <url|title|text|html|value|attr|count> [target] [attr-name]
    Get {
        what: String,
        #[arg(num_args = 0..=2)]
        args: Vec<String>,
        #[arg(long, alias = "selector")]
        target_selector: Option<String>,
        #[arg(long, alias = "attr")]
        name: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// console [list|clear]
    Console {
        #[arg(value_parser = ["list", "clear"])]
        action: Option<String>,
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// errors [list|clear]
    Errors {
        #[arg(value_parser = ["list", "clear"])]
        action: Option<String>,
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        target: BrowserTarget,
    },
    /// tab <new|list|switch|close|<index>> [index|url]
    Tab {
        action: String,
        arg: Option<String>,
        #[arg(long)]
        index: Option<u64>,
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        target: BrowserTarget,
    },
}

#[derive(Subcommand, Debug)]
pub enum HarnessCmd {
    /// Query observed model/effort, or request a change. Unsupported control
    /// returns an explicit status and leaves the session untouched.
    #[command(name = "harness-config")]
    Config {
        #[command(flatten)]
        target: Target,
        /// Request this session model. Requires supported owning-harness control.
        #[arg(long)]
        model: Option<String>,
        /// Request this model's advertised effort ID (for example high).
        #[arg(long)]
        effort: Option<String>,
        /// Print available session-specific choices, when observed through ACP.
        #[arg(long, conflicts_with_all = ["model", "effort"])]
        options: bool,
    },
    /// Query per-surface harness lifecycle, queue and delivery evidence.
    #[command(name = "harness-status")]
    Status {
        #[command(flatten)]
        target: Target,
        /// Query all terminal surfaces. A workspace target queries its terminals.
        #[arg(long, conflicts_with_all = ["surface", "workspace"])]
        all: bool,
        /// Return cached evidence, with staleness, without capturing a screen.
        #[arg(long)]
        cached: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum HooksCmd {
    /// Forward content-free Grok observations; print-config never installs hooks.
    Grok {
        #[command(subcommand)]
        cmd: GrokHook,
    },
    /// Install/print Claude Code hooks.
    Claude {
        #[command(subcommand)]
        cmd: ClaudeHook,
    },
}

#[derive(Subcommand, Debug)]
pub enum GrokHook {
    /// Read a supported Grok hook event from stdin; print nothing.
    Event,
    /// Forward a supported status-line payload from stdin; print nothing.
    StatusLine,
    /// Print an opt-in Grok hooks snippet without editing settings.
    PrintConfig,
}

#[derive(Subcommand, Debug)]
pub enum ClaudeHook {
    SessionStart,
    PromptSubmit,
    Stop,
    Notification,
    /// Print the settings.json snippet (does not edit anything).
    PrintConfig,
}

/// Global flags that take a value, for [`rewrite_browser_surface`].
const VALUE_GLOBALS: &[&str] = &["--socket", "--host"];

/// Stands in for the `--` that ends `send`/`paste` flags. clap swallows a
/// `--` before the text but keeps one after it, so the text alone cannot say
/// where flags stop; [`mark_end_of_flags`] makes it explicit.
pub const END_OF_FLAGS: &str = "\u{1}end-of-flags";

/// The index of the subcommand word: the first that is not a global flag.
fn subcommand_index(args: &[String]) -> usize {
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if VALUE_GLOBALS.contains(&a.as_str()) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// For `send`/`paste`, replace the first `--` with [`END_OF_FLAGS`].
pub fn mark_end_of_flags(mut args: Vec<String>) -> Vec<String> {
    let i = subcommand_index(&args);
    if matches!(args.get(i).map(String::as_str), Some("send" | "paste")) {
        if let Some(dd) = args.iter().skip(i + 1).position(|a| a == "--") {
            args[i + 1 + dd] = END_OF_FLAGS.to_string();
        }
    }
    args
}

/// cmux takes a browser surface before the subcommand, as
/// `browser surface:4 snapshot` or `browser --surface surface:4 snapshot`;
/// clap wants flags on the subcommand. Move such a surface (and a
/// `--workspace`) after it.
pub fn rewrite_browser_surface(args: Vec<String>, subcommands: &[String]) -> Vec<String> {
    // Find `browser` as the first word that is not a global flag.
    let i = subcommand_index(&args);
    if args.get(i).map(String::as_str) != Some("browser") {
        return args;
    }
    let mut j = i + 1;
    let mut surface = None;
    let mut workspace = None;
    let mut globals = Vec::new();
    loop {
        match args.get(j).map(String::as_str) {
            Some(flag) if VALUE_GLOBALS.contains(&flag) && j + 1 < args.len() => {
                globals.extend_from_slice(&args[j..j + 2]);
                j += 2;
            }
            Some(flag) if flag == "--json" || VALUE_GLOBALS.iter().any(|global| flag.starts_with(&format!("{global}="))) => {
                globals.push(flag.to_string());
                j += 1;
            }
            Some("--workspace") if j + 1 < args.len() => {
                workspace = Some(args[j + 1].clone());
                j += 2;
            }
            Some(a) if a.starts_with("--workspace=") => {
                workspace = Some(a["--workspace=".len()..].to_string());
                j += 1;
            }
            Some("--surface") if j + 1 < args.len() => {
                surface = Some(args[j + 1].clone());
                j += 2;
            }
            Some(a) if a.starts_with("--surface=") => {
                surface = Some(a["--surface=".len()..].to_string());
                j += 1;
            }
            Some(a) if surface.is_none() && !a.starts_with('-') && !subcommands.iter().any(|s| s == a) => {
                surface = Some(a.to_string());
                j += 1;
            }
            _ => break,
        }
    }
    if surface.is_none() && workspace.is_none() {
        return args;
    }
    let mut out: Vec<String> = args[..=i].to_vec();
    if let Some(sub) = args.get(j) {
        out.push(sub.clone());
        out.extend(globals);
        for (flag, v) in [("--surface", surface), ("--workspace", workspace)] {
            if let Some(v) = v {
                out.push(flag.into());
                out.push(v);
            }
        }
        out.extend(args[j + 1..].iter().cloned());
    } else {
        // No subcommand: leave it for clap to report.
        out.extend(args[i + 1..].iter().cloned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn browser_subcommands() -> Vec<String> {
        let cmd = Cli::command();
        let b = cmd.find_subcommand("browser").unwrap();
        b.get_subcommands()
            .flat_map(|s| std::iter::once(s.get_name().to_string()).chain(s.get_all_aliases().map(String::from)))
            .collect()
    }

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn host_flags_preserve_browser_rewriting_and_literal_send_text() {
        let subs = browser_subcommands();
        let args = rewrite_browser_surface(v("lmux --host box browser surface:4 snapshot -i"), &subs);
        assert_eq!(args, v("lmux --host box browser snapshot --surface surface:4 -i"));
        assert_eq!(Cli::try_parse_from(args).unwrap().host.as_deref(), Some("box"));
        assert_eq!(Cli::try_parse_from(v("lmux tree --host=box")).unwrap().host.as_deref(), Some("box"));
        for args in ["lmux browser surface:4 --host box snapshot -i", "lmux browser --host=box surface:4 snapshot -i", "lmux browser --host box --surface surface:4 snapshot -i"] {
            let cli = Cli::try_parse_from(rewrite_browser_surface(v(args), &subs)).unwrap();
            assert_eq!(cli.host.as_deref(), Some("box"));
            assert!(matches!(cli.cmd, Cmd::Browser { cmd: BrowserCmd::Snapshot { target, .. } } if target.surface.as_deref() == Some("surface:4")));
        }
        let cli = Cli::try_parse_from(mark_end_of_flags(v("lmux --host box send -- --host literal"))).unwrap();
        assert_eq!(cli.host.as_deref(), Some("box"));
        assert!(matches!(cli.cmd, Cmd::Send { text, .. } if text == vec![END_OF_FLAGS, "--host", "literal"]));
        for args in ["lmux --host box tree --all-hosts", "lmux tree --all-hosts --workspace 1", "lmux --socket /s tree --all-hosts"] {
            if let Ok(cli) = Cli::try_parse_from(v(args)) {
                assert!(cli.validate_host_scope().is_err(), "{args}");
            }
        }
    }

    #[test]
    fn clap_definitions_are_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn top_level_help_lists_every_command() {
        let cmd = Cli::command();
        let listed: Vec<&str> = COMMANDS_HELP
            .lines()
            .filter(|l| l.starts_with("  "))
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
            assert!(listed.contains(&sub.get_name()), "{} missing from COMMANDS_HELP", sub.get_name());
        }
        for name in &listed {
            assert!(
                *name == "help" || cmd.find_subcommand(name).is_some(),
                "COMMANDS_HELP lists unknown command {name}"
            );
        }
    }

    #[test]
    fn browser_surface_moves_after_the_subcommand() {
        let subs = browser_subcommands();
        assert_eq!(
            rewrite_browser_surface(v("lmux browser surface:4 snapshot -i"), &subs),
            v("lmux browser snapshot --surface surface:4 -i")
        );
        assert_eq!(
            rewrite_browser_surface(v("lmux --socket /s --json browser --surface surface:4 goto x"), &subs),
            v("lmux --socket /s --json browser goto --surface surface:4 x")
        );
        assert_eq!(
            rewrite_browser_surface(v("lmux browser --surface=surface:4 url"), &subs),
            v("lmux browser url --surface surface:4")
        );
        assert_eq!(
            rewrite_browser_surface(v("lmux browser --workspace 2 get title"), &subs),
            v("lmux browser get --workspace 2 title")
        );
        // Either order parses, for every subcommand.
        for s in ["lmux browser --workspace 2 get title", "lmux browser get title --workspace 2", "lmux browser open --workspace w x"] {
            let args = rewrite_browser_surface(v(s), &subs);
            assert!(Cli::try_parse_from(&args).is_ok(), "{s}");
        }
        // Already in clap's order, or not a browser command: unchanged.
        for s in ["lmux browser snapshot --surface surface:4", "lmux send browser", "lmux browser open"] {
            assert_eq!(rewrite_browser_surface(v(s), &subs), v(s));
        }
    }

    #[test]
    fn send_marks_only_its_first_double_dash() {
        let out = mark_end_of_flags(v("lmux --json send --surface s -- -x -- y"));
        assert_eq!(out[3], "--surface");
        assert_eq!(out[5], END_OF_FLAGS);
        assert_eq!(out[7], "--");
        assert_eq!(mark_end_of_flags(v("lmux log -- -x")), v("lmux log -- -x"));
        // The marker parses as text, so the flags before it still apply.
        let cli = Cli::try_parse_from(mark_end_of_flags(v("lmux send --surface s -- --surface y"))).unwrap();
        match cli.cmd {
            Cmd::Send { text, target, .. } => {
                assert_eq!(target.surface.as_deref(), Some("s"));
                assert_eq!(text, vec![END_OF_FLAGS.to_string(), "--surface".into(), "y".into()]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cmux_spellings_parse() {
        let ok = [
            "lmux notify --title T --body B",
            "lmux notify T",
            "lmux rename-workspace --workspace workspace:1 new name",
            "lmux rename-window x",
            "lmux select-workspace --workspace workspace:2",
            "lmux close-workspace --workspace workspace:2 --force",
            "lmux new-split right --panel surface:1 --focus false",
            "lmux new-pane --type browser --direction down --url https://x",
            "lmux new-pane --side left",
            "lmux new-surface --working-directory /tmp",
            "lmux focus-pane --pane pane:2",
            "lmux focus-pane pane:2",
            "lmux swap-pane --pane pane:1 --target-pane pane:2",
            "lmux swap-pane pane:1 pane:2",
            "lmux move-surface --surface surface:1 --workspace workspace:2",
            "lmux split-off --surface surface:1 right",
            "lmux split-surface surface:1 --side right",
            "lmux drag-surface-to-split --panel surface:2 down",
            "lmux detach-tab --tab surface:3 --title logs",
            "lmux rename-tab --surface surface:2 build logs",
            "lmux send --surface surface:2 ls -la\\n",
            "lmux send-key --surface surface:2 ctrl+c",
            "lmux read-screen --workspace workspace:2 --lines 20",
            "lmux capture-pane -p -S -100 -E - -t surface:2",
            "lmux capture-pane --surface surface:1 --scrollback --lines 200",
            "lmux wait-for -S build-done",
            "lmux wait-for build-done --timeout 5",
            "lmux wait-for --text $ --surface surface:1 --timeout 2",
            "lmux clear-history --surface surface:1",
            "lmux dismiss-notification --all-read",
            "lmux mark-notification-read --all",
            "lmux set-status build compiling --icon hammer --color #ff9500 --priority 80",
            "lmux log --level error --source build -- Compilation failed",
            "lmux list-log --limit 5",
            "lmux reorder-workspace --workspace workspace:2 --after workspace:1",
            "lmux browser open https://example.com --workspace workspace:1",
            "lmux browser navigate https://example.com",
            "lmux browser get-url",
            "lmux browser click --selector #go",
            "lmux browser type #q hello world",
            "lmux browser wait --url /done --timeout 5",
            "lmux browser console list",
            "lmux browser tab 2",
            "lmux rpc system.tree",
            "lmux rpc term.read_screen {\"lines\":5}",
        ];
        for s in ok {
            let args: Vec<String> = v(s);
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{s}: {e}");
            }
        }
    }
}
