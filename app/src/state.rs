//! Client state: the session tree plus per-surface terminal/browser state, and
//! the subscribe/unsubscribe bookkeeping that keeps hidden panes idle.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use egui::ColorImage;
use lmux_proto::{Layout, Notification, Pane, ServerMsg, Surface, SurfaceKind, Tree};

use crate::pty::LocalPty;
use crate::term::TermView;
use crate::wire::{self, m, ClientMsg, Ref};

/// How long to wait before attaching a local pane again after its tmux client
/// died, so a tmux that cannot serve the pane is not respawned every frame.
const LOCAL_PTY_RETRY: Duration = Duration::from_secs(1);
/// How long a hidden streamed terminal keeps its view. Showing it again
/// within this is instant; later, it comes back from a fresh snapshot.
const HIDDEN_VIEW_TTL: Duration = Duration::from_secs(60);

/// What to do with the reply to a browser request.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserReply {
    /// `surface.new` for a blank tab: focus its address bar.
    FocusAddressBar,
    /// An eval returning the page's selected text: put it on the clipboard.
    Copy,
    /// `browser.link_at`: open the link, if any, as a new tab in `pane`.
    OpenLink { pane: Ref },
}

pub struct BrowserView {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_back: bool,
    pub can_fwd: bool,
    pub texture: Option<egui::TextureHandle>,
    pub pending_image: Option<ColorImage>,
    pub frame_size: (u16, u16),
    pub seq: u32,
    /// Decoded but not yet acknowledged frame; acked after its texture upload.
    pub pending_ack: Option<u32>,
    pub wants_visible: bool,
    pub subscribed: bool,
    /// The size last sent to the daemon. Cleared with `subscribed`, so a new
    /// subscription (after a reconnect) sends it again.
    pub sent_size: Option<(u32, u32, f32)>,
    /// A size change waiting out the resize debounce (review #9).
    pub pending_size: Option<(u32, u32, f32)>,
    pub pending_since: Option<std::time::Instant>,
    pub url_draft: Option<String>,
    /// Focus the address bar and select its text on the next draw (Cmd+L, a
    /// new tab).
    pub focus_url: bool,
    pub have_frame: bool,
}

/// A size the browser pane wants the daemon to stream at: CSS width, height and
/// device scale factor.
pub type BrowserSize = (u32, u32, f32);

impl BrowserView {
    pub fn new(url: String) -> Self {
        Self {
            url,
            title: String::new(),
            loading: false,
            can_back: false,
            can_fwd: false,
            texture: None,
            pending_image: None,
            frame_size: (0, 0),
            seq: 0,
            pending_ack: None,
            wants_visible: false,
            subscribed: false,
            sent_size: None,
            pending_size: None,
            pending_since: None,
            url_draft: None,
            focus_url: false,
            have_frame: false,
        }
    }

    /// Decide whether to report `want` now, or wait out the debounce window.
    /// Returns `(send_now, repaint_after)`. A resize storm keeps restarting the
    /// window, so only the settled size is sent (review #9). The very first size
    /// is sent immediately so the stream starts without a debounce delay.
    pub fn resize_decision(
        &mut self,
        want: BrowserSize,
        now: std::time::Instant,
        debounce: std::time::Duration,
    ) -> (bool, Option<std::time::Duration>) {
        if !self.subscribed || self.sent_size == Some(want) {
            self.pending_size = None;
            self.pending_since = None;
            return (false, None);
        }
        let mut send = false;
        let mut schedule = None;
        match self.pending_size {
            None if self.sent_size.is_none() => send = true,
            Some(p) if p == want => match self.pending_since {
                Some(t) if now.saturating_duration_since(t) >= debounce => send = true,
                Some(t) => schedule = Some(debounce.saturating_sub(now.saturating_duration_since(t))),
                None => send = true,
            },
            _ => {
                self.pending_size = Some(want);
                self.pending_since = Some(now);
                schedule = Some(debounce);
            }
        }
        if send {
            self.sent_size = Some(want);
            self.pending_size = None;
            self.pending_since = None;
        }
        (send, schedule)
    }
}

/// Lines of tmux history fetched when a streamed terminal is scrolled back.
pub const STREAMED_HISTORY: u32 = 3000;

/// What the toolbar asks of a browser the app draws itself (`crate::webview`).
#[derive(Debug, Clone, PartialEq)]
pub enum NativeCmd {
    Back,
    Forward,
    Reload,
    Stop,
    Goto(String),
}

/// A step of an agent's browser command, from the daemon, for a browser the
/// app draws; answered with `ClientMsg::AppBrowserResult`.
#[derive(Debug, Clone)]
pub struct AppCall {
    pub id: u64,
    pub surface: Ref,
    pub op: lmux_proto::AppBrowserOp,
    pub since: Instant,
}

pub struct AppState {
    pub tree: Tree,
    /// The host the tree came from, so a create can be aimed at another one.
    pub current_host: String,
    /// Folder lists of the other hosts, from their last tree. The create menu
    /// offers every host's folders, not only the connected one's.
    pub host_folders: HashMap<String, Vec<lmux_proto::Folder>>,
    /// A create aimed at a host that is not connected yet; it fires when that
    /// host's tree arrives.
    pub pending_create: Option<PendingCreate>,
    /// Salts this host's egui ids, so two hosts that both have `workspace:1`
    /// never share a widget id in the one sidebar. Empty for a single host.
    pub host_salt: String,
    /// Set when the user acted on this host (selected or created something),
    /// so the app makes it the host on screen. Cleared by the app each frame.
    pub activated: bool,
    /// The sidebar shows this host's group folded to its header.
    pub host_collapsed: bool,
    /// The "Add Remote Host" field, while it is open: the SSH target typed so far.
    pub add_host_draft: Option<String>,
    /// Whether adding the host also installs lmux there (on by default).
    pub add_host_install: bool,
    /// An install of lmux on this host, while running or just finished.
    pub install: Option<std::sync::Arc<std::sync::Mutex<crate::net::InstallStatus>>>,
    /// The "New Cloud Box" dialog, while it is open.
    pub cloud_draft: Option<CloudDraft>,
    /// The "Import from cmux" dialog, while it is open.
    pub cmux_import: Option<crate::cmux_import::CmuxImport>,
    /// The cloud box whose deletion is waiting for a yes: its host's name.
    pub confirm_cloud_delete: Option<String>,
    /// This host's cloud box being deleted; the host goes when it is done.
    pub cloud_delete: Option<std::sync::Arc<std::sync::Mutex<crate::net::InstallStatus>>>,
    pub terms: HashMap<Ref, TermView>,
    pub browsers: HashMap<Ref, BrowserView>,
    /// PTYs owned by this process, one per local terminal surface. Present only
    /// while the connected host is this machine (see [`AppState::local_mode`]).
    pub local_ptys: HashMap<Ref, LocalPty>,
    /// Focus in/out last reported to each terminal that asked for focus
    /// events (mode 1004). Missing means focused: what programs assume, and
    /// what a fresh tmux client reports.
    pub focus_reported: HashMap<Ref, bool>,
    /// True while the connected host is the machine the app runs on. Terminal
    /// panes then attach through a local PTY instead of the daemon's stream.
    pub local_mode: bool,
    /// Local surfaces the user has shown; their PTY is kept (and retried) even
    /// while the tab is hidden, so switching back is instant.
    local_wanted: HashSet<Ref>,
    /// Earliest next attach attempt per surface, so a failing tmux cannot be
    /// respawned every frame.
    local_retry: HashMap<Ref, Instant>,
    /// egui context, used to wake the UI when a local PTY delivers output.
    wake: Option<egui::Context>,
    /// This host's browser frame decoder, started with its first frame.
    decoder: Option<crate::decode::Decoder>,
    pub notifications: Vec<Notification>,
    pub selected: Option<Ref>,
    /// Local tab selection per pane (the protocol has no activate-surface RPC).
    pub pane_active: HashMap<Ref, Ref>,

    // UI state
    pub sidebar_w: f32,
    pub sidebar_hidden: bool,
    pub collapsed: HashSet<Ref>,
    pub notif_open: bool,
    pub ws_history: Vec<Ref>,
    pub ws_history_pos: usize,
    pub dragging: Option<Drag>,
    /// In-flight chip/pane-header drag (reorder, move, split, swap).
    pub tab_drag: Option<TabDrag>,
    /// The target the in-flight drag resolves to this frame (for feedback).
    pub tab_drop: Option<DropTarget>,
    pub renaming: Option<Rename>,
    /// Inline rename of a folder row.
    pub folder_rename: Option<FolderRename>,
    /// Inline rename of a workspace card's title (`folder` holds the workspace).
    pub ws_rename: Option<FolderRename>,
    /// Closes sent with `if_idle`, by RPC id, until the daemon answers.
    pending_closes: HashMap<u64, CloseTarget>,
    /// The "still running" question, while it is open.
    pub confirm_close: Option<ConfirmClose>,
    /// Clipboard images being read for a paste: the surface, and the PNG's
    /// local path once read (None: the clipboard held text or no image).
    image_reads: Vec<(Ref, std::sync::mpsc::Receiver<Option<std::path::PathBuf>>)>,
    /// Images uploaded to the host for a paste, by RPC id: the surface.
    pending_uploads: HashMap<u64, Ref>,
    /// Animate (the running-agent spinner). The app turns it on; tests keep
    /// still frames.
    pub animate: bool,
    /// While Cmd is held, cards of the host on screen show their ⌘1…9 key.
    pub digit_hints: bool,
    /// The pane shown alone over its workspace (Cmd+Shift+Enter), if any.
    pub zoomed_pane: Option<Ref>,
    /// The command palette, while it is open.
    pub palette: Option<Palette>,
    /// Terminal font size in points (Cmd+= / Cmd+- / Cmd+0).
    pub term_font: f32,
    /// The collapse flip the folder row's last click made, for the double click
    /// that follows it.
    pub folder_toggle: Option<FolderToggle>,
    /// `folder.new` requests whose result should open the rename editor.
    folder_rename_pending: Option<u64>,
    /// Browser requests whose reply the app acts on, by RPC id.
    browser_replies: HashMap<u64, BrowserReply>,
    /// Text for the system clipboard, put there on the next frame (the reply to
    /// a browser copy arrives without an egui context at hand).
    pub clipboard_out: Option<String>,
    pub last_width: f32,

    // connection/status
    pub host: String,
    pub server_version: String,
    pub banner: Option<(String, bool)>,

    next_rpc: u64,
    /// Messages queued during a frame, flushed by the app.
    pub pending: Vec<ClientMsg>,
    /// Surface sizes the UI wants, keyed by surface ref.
    pub desired_sizes: HashMap<Ref, (u16, u16)>,
    /// Surfaces visible this frame.
    pub visible: HashSet<Ref>,
    /// Where each native browser goes this frame, in points; placed by
    /// `crate::webview` after the frame.
    pub native_rects: HashMap<Ref, egui::Rect>,
    /// Toolbar commands for native browsers, run by `crate::webview`.
    pub native_cmds: Vec<(Ref, NativeCmd)>,
    /// An agent's browser steps for native browsers, run by `crate::webview`.
    pub app_calls: Vec<AppCall>,
    /// Where each chip was drawn last frame, keyed by surface. Kept so tests can
    /// click the real chip geometry rather than recomputing the layout.
    pub chip_rects: HashMap<Ref, egui::Rect>,
    /// Where each sidebar workspace card was drawn last frame, keyed by
    /// workspace. Same purpose as `chip_rects`.
    pub card_rects: HashMap<Ref, egui::Rect>,
    /// Where each sidebar folder row was drawn last frame, keyed by folder.
    pub folder_rects: HashMap<Ref, egui::Rect>,
    /// Every sidebar row drawn last frame, in draw order. A row drag resolves
    /// against this list, so its indicator names the row the daemon will act on.
    pub sidebar_rows: Vec<SidebarRow>,
    /// The sidebar's list area, below the top icon strip. A row drop only counts
    /// inside it, so a drag across the panes cannot land in the sidebar.
    pub sidebar_list_rect: Option<egui::Rect>,
    /// Where each folder row's `+` (a new session in that folder) was drawn.
    pub folder_plus_rects: HashMap<Ref, egui::Rect>,
    /// Where each pane was drawn last frame, keyed by pane. Used to hit-test
    /// drags against the geometry that is actually on screen.
    pub pane_rects: HashMap<Ref, egui::Rect>,
    /// Where each browser frame was drawn last frame, keyed by surface. Same
    /// purpose: pointer tests drive the real geometry.
    pub browser_image_rects: HashMap<Ref, egui::Rect>,
    /// OS notifications to deliver when the window is unfocused.
    pub os_queue: Vec<Notification>,
    /// UI intents for the app to apply after the frame.
    pub actions: Vec<Action>,
}

/// UI intent that the app applies after the frame (net access).
#[derive(Clone, Debug)]
pub enum Action {
    SwitchHost(usize),
    Reconnect,
    Quit,
    /// Add a remote host reached over SSH by this target, and connect to it.
    AddHost(String),
    /// Forget this host (the one whose state queued the action).
    RemoveHost,
    /// Install or update lmux on this host over SSH, then reconnect.
    InstallHost,
    /// Create a cloud box, install lmux on it and add it as a host.
    CreateCloud(CloudDraft),
    /// Delete the cloud box behind the host with this name, then forget it.
    DeleteCloud(String),
}

/// The fields of the "New Cloud Box" dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct CloudDraft {
    /// `aws` or `gcp`.
    pub provider: String,
    pub name: String,
    /// Empty means the CLI's configured default.
    pub region: String,
    pub size: String,
}

impl CloudDraft {
    pub fn new(provider: &str) -> Self {
        CloudDraft {
            provider: provider.to_owned(),
            name: String::new(),
            region: String::new(),
            size: Self::default_size(provider).to_owned(),
        }
    }

    pub fn default_size(provider: &str) -> &'static str {
        if provider == "gcp" {
            "e2-standard-2"
        } else {
            "t3.medium"
        }
    }

    /// Why the name can't be used, if it can't: it becomes a DNS-style label
    /// (`lmux-<name>`) on both clouds.
    pub fn name_problem(&self) -> Option<&'static str> {
        let n = &self.name;
        if n.is_empty() {
            return Some("pick a name");
        }
        if n.len() > 40 {
            return Some("at most 40 characters");
        }
        if !n.starts_with(|c: char| c.is_ascii_lowercase()) {
            return Some("start with a lowercase letter");
        }
        if !n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') || n.ends_with('-') {
            return Some("lowercase letters, digits and dashes only");
        }
        None
    }
}

/// Something a close was asked for, kept until the daemon answers.
#[derive(Clone, Debug, PartialEq)]
pub enum CloseTarget {
    /// `(pane, surface, the pane's tab selection before the close)`.
    Surface(Ref, Ref, Option<Ref>),
    Workspace(Ref),
}

/// A close the daemon refused because a program is still running there.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmClose {
    pub target: CloseTarget,
    pub program: String,
}

/// What the palette lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    /// Cmd+Shift+P: commands.
    Commands,
    /// Cmd+P: every workspace on every host.
    Workspaces,
}

/// The open command palette.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub mode: PaletteMode,
    pub query: String,
    /// The highlighted row in the filtered list.
    pub index: usize,
}

/// Inline rename of a surface chip.
#[derive(Clone, Debug, PartialEq)]
pub struct Rename {
    pub surface: Ref,
    pub text: String,
    /// Focus is requested on the first frame the editor exists.
    pub focus_requested: bool,
}

/// Inline rename of a folder row.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderRename {
    pub folder: Ref,
    pub text: String,
    pub focus_requested: bool,
}

/// What a create aimed at another host should make once that host answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateKind {
    Session,
    Folder,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingCreate {
    pub host: String,
    pub kind: CreateKind,
    /// The folder it goes in, from that host's own folder list.
    pub folder: Option<Ref>,
}

/// One host a new session or folder can be created in, with the folders the app
/// knows for it.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateTarget {
    pub host: String,
    pub index: usize,
    /// The connected host, which is the only one that can be created in today.
    pub current: bool,
    /// `(folder ref, folder trail)`; empty for a host never seen.
    pub folders: Vec<(Ref, String)>,
}

/// The collapse flip a folder row's click just made: which folder, what the
/// state was before, and when. A double click inside egui's own double-click
/// window takes the flip back, so renaming a folder does not collapse it.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderToggle {
    pub folder: Ref,
    pub collapsed_before: bool,
    pub at: f64,
}

#[derive(Clone, Debug)]
pub struct Drag {
    pub path: Vec<u8>,
    pub direction: lmux_proto::SplitDirection,
    pub region: egui::Rect,
    /// Pane to report in `pane.resize` (the first leaf under the split).
    pub pane: Ref,
    /// Live ratio while dragging, applied as a local override.
    pub ratio: f32,
}

/// What a chip/header/sidebar-row drag is carrying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragItem {
    /// One surface chip.
    Surface(Ref),
    /// A whole pane, grabbed by its strip.
    Pane(Ref),
    /// A sidebar workspace card (a session row).
    Workspace(Ref),
    /// A sidebar folder row.
    Folder(Ref),
}

/// One row of the sidebar as it was drawn this frame: the folder rows and
/// workspace cards in draw order. Drags resolve against this list, so the
/// indicator can only ever sit on a row that is on screen.
#[derive(Clone, Debug, PartialEq)]
pub struct SidebarRow {
    pub id: Ref,
    pub kind: SidebarRowKind,
    pub rect: egui::Rect,
    /// Where the row lives: a workspace card's folder, a folder row's parent.
    /// `None` is the top level.
    pub in_folder: Option<Ref>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarRowKind {
    Folder,
    Workspace,
}

impl DragItem {
    /// Whether this drag carries a sidebar folder row rather than a session.
    pub fn item_is_folder(&self) -> bool {
        matches!(self, DragItem::Folder(_))
    }
}

/// An in-flight drag of a chip or a pane header. Distinct from [`Drag`], which
/// is the divider resize. The geometry the UI actually drew is looked up via
/// [`AppState::chip_rects`] / [`AppState::pane_rects`] so hit-testing matches
/// what is on screen.
#[derive(Clone, Debug)]
pub struct TabDrag {
    pub item: DragItem,
    /// The pane the drag started in.
    pub source_pane: Ref,
    /// Pointer offset within the grabbed rect, so the ghost sits under the cursor.
    pub grab: egui::Vec2,
    /// Size of the ghost.
    pub size: egui::Vec2,
    /// Pointer position this frame.
    pub pointer: egui::Pos2,
}

/// Where an in-flight drag would land.
#[derive(Clone, Debug, PartialEq)]
pub enum DropTarget {
    /// Insert the dragged surface into `pane`; `index` is its position in that
    /// pane's surface list (`None` = append at the end).
    Move { pane: Ref, index: Option<usize> },
    /// Split `pane` on `side` (`left|right|up|down`) and put the dragged surface
    /// in the new pane. `pane` is the one under the pointer, which is also the
    /// one the indicator is drawn on, so the tab lands where the user aimed.
    Split { pane: Ref, side: &'static str },
    /// Exchange the dragged pane with `other`.
    Swap { other: Ref },
    /// A sidebar row dropped into `into`: the folder a workspace joins, or the
    /// folder a dragged folder nests inside (`None` = the top level). `slot`
    /// counts that container's rows, so the RPC's `index` is derived from it.
    /// The feedback is one of two: an accent line at `line_y` for a slot
    /// insertion, or the `adopt` folder row filled when the drop puts the row
    /// inside a folder rather than between two rows.
    Sidebar {
        into: Option<Ref>,
        slot: Option<usize>,
        adopt: Option<Ref>,
        line_y: Option<f32>,
    },
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            tree: Tree::default(),
            current_host: String::new(),
            host_folders: HashMap::new(),
            host_salt: String::new(),
            activated: false,
            host_collapsed: false,
            add_host_draft: None,
            add_host_install: true,
            cloud_draft: None,
            cmux_import: None,
            confirm_cloud_delete: None,
            cloud_delete: None,
            install: None,
            pending_create: None,
            terms: HashMap::new(),
            browsers: HashMap::new(),
            local_ptys: HashMap::new(),
            focus_reported: HashMap::new(),
            local_mode: false,
            local_wanted: HashSet::new(),
            local_retry: HashMap::new(),
            wake: None,
            decoder: None,
            notifications: Vec::new(),
            selected: None,
            pane_active: HashMap::new(),
            sidebar_w: crate::theme::SIDEBAR_DEFAULT_W,
            sidebar_hidden: false,
            collapsed: HashSet::new(),
            notif_open: false,
            ws_history: Vec::new(),
            ws_history_pos: 0,
            dragging: None,
            tab_drag: None,
            tab_drop: None,
            renaming: None,
            folder_rename: None,
            ws_rename: None,
            palette: None,
            zoomed_pane: None,
            digit_hints: false,
            animate: false,
            pending_closes: HashMap::new(),
            browser_replies: HashMap::new(),
            clipboard_out: None,
            confirm_close: None,
            image_reads: Vec::new(),
            pending_uploads: HashMap::new(),
            term_font: crate::theme::FS_TERM,
            folder_toggle: None,
            folder_rename_pending: None,
            last_width: 0.0,
            host: String::new(),
            server_version: String::new(),
            banner: None,
            next_rpc: 0,
            pending: Vec::new(),
            desired_sizes: HashMap::new(),
            visible: HashSet::new(),
            native_rects: HashMap::new(),
            native_cmds: Vec::new(),
            app_calls: Vec::new(),
            chip_rects: HashMap::new(),
            card_rects: HashMap::new(),
            folder_rects: HashMap::new(),
            sidebar_rows: Vec::new(),
            sidebar_list_rect: None,
            folder_plus_rects: HashMap::new(),
            pane_rects: HashMap::new(),
            browser_image_rects: HashMap::new(),
            os_queue: Vec::new(),
            actions: Vec::new(),
        }
    }

    // --- rpc ---------------------------------------------------------------

    pub fn rpc(&mut self, method: &str, params: serde_json::Value) {
        self.rpc_with_id(method, params);
    }

    /// Like [`Self::rpc`], but returns the request id so a reply can be matched.
    pub fn rpc_with_id(&mut self, method: &str, params: serde_json::Value) -> u64 {
        self.next_rpc += 1;
        let id = self.next_rpc;
        self.pending.push(wire::rpc(id, method, params));
        id
    }

    pub fn rpc_empty(&mut self, method: &str) {
        self.next_rpc += 1;
        let id = self.next_rpc;
        self.pending.push(wire::rpc_empty(id, method));
    }

    // --- creating ----------------------------------------------------------

    /// A new session (workspace) inside `folder`, or at top level with `None`.
    /// The session is a terminal in the sidebar, running on the connected host.
    pub fn new_session(&mut self, folder: Option<Ref>) {
        self.activated = true;
        self.rpc(
            m::WORKSPACE_NEW,
            // No command: the daemon runs the shell tmux defaults to.
            serde_json::json!({ "name": "workspace", "folder": folder }),
        );
    }

    /// A new folder, nested inside `parent` when one is given. Its rename
    /// editor opens as soon as the daemon reports the new ref, so the folder
    /// gets a name in the same gesture that created it.
    pub fn new_folder(&mut self, parent: Option<Ref>) {
        self.activated = true;
        let mut params = serde_json::json!({ "name": "New Folder" });
        if let Some(p) = parent {
            params["parent"] = serde_json::json!(p);
        }
        self.folder_rename_pending = Some(self.rpc_with_id(m::FOLDER_NEW, params));
    }

    /// The folder a bare "new session" on the sidebar should land in: the one
    /// holding the selected workspace, so a session appears where you are.
    pub fn default_new_folder(&self) -> Option<Ref> {
        self.selected_workspace().and_then(|w| w.folder.clone())
    }

    pub fn begin_folder_rename(&mut self, folder: &str, current: &str) {
        self.folder_rename = Some(FolderRename {
            folder: folder.to_owned(),
            text: current.to_owned(),
            focus_requested: false,
        });
    }

    /// Flip a folder row's collapse state, remembering the state before the
    /// flip so the double click that may follow can take it back.
    pub fn toggle_folder(&mut self, folder: &str, now: f64) {
        let collapsed_before = self.collapsed.contains(folder);
        self.folder_toggle = Some(FolderToggle {
            folder: folder.to_owned(),
            collapsed_before,
            at: now,
        });
        if collapsed_before {
            self.collapsed.remove(folder);
        } else {
            self.collapsed.insert(folder.to_owned());
        }
    }

    /// Take back the flip [`Self::toggle_folder`] made on `folder`, when this
    /// double click arrives within `window` of that click. A double click is
    /// an edit, not a collapse, so the row must end where it started. Reports
    /// whether a flip was taken back.
    pub fn take_back_folder_toggle(&mut self, folder: &str, window: f64, now: f64) -> bool {
        let Some(toggle) = self.folder_toggle.take() else { return false };
        if toggle.folder != folder || now - toggle.at > window {
            return false;
        }
        if toggle.collapsed_before {
            self.collapsed.insert(folder.to_owned());
        } else {
            self.collapsed.remove(folder);
        }
        true
    }

    // --- lookups -----------------------------------------------------------

    pub fn workspace(&self, id: &str) -> Option<&lmux_proto::Workspace> {
        self.tree.workspaces.iter().find(|w| w.id == id)
    }

    pub fn folder(&self, id: &str) -> Option<&lmux_proto::Folder> {
        self.tree.folders.iter().find(|f| f.id == id)
    }

    /// The folders directly inside `parent` (`None` = top level).
    pub fn folder_children(&self, parent: Option<&str>) -> Vec<&lmux_proto::Folder> {
        self.tree
            .folders
            .iter()
            .filter(|f| f.parent.as_deref() == parent)
            .collect()
    }

    pub fn folder_has_children(&self, id: &str) -> bool {
        self.tree.folders.iter().any(|f| f.parent.as_deref() == Some(id))
    }

    /// A folder's trail, `webapp / api`, for menus that name the target.
    pub fn folder_path(&self, id: &str) -> String {
        folder_path_in(&self.tree.folders, id)
    }

    // --- hosts -------------------------------------------------------------

    /// Record which host the tree currently belongs to, and remember that
    /// host's folder list. Called as messages arrive.
    pub fn set_current_host(&mut self, host: &str) {
        if self.current_host != host {
            self.current_host = host.to_owned();
        }
    }

    /// Every host a session or folder can be created in, with the folders known
    /// for each: the live tree for the connected host, the cached list for the
    /// others. Empty-folder hosts still appear, so a first session can be made
    /// anywhere.
    pub fn create_targets(
        &self,
        hosts: &[crate::net::HostConfig],
        current: usize,
    ) -> Vec<CreateTarget> {
        hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                let name = host.label.clone();
                let folders = if index == current {
                    self.tree.folders.clone()
                } else {
                    self.host_folders.get(&name).cloned().unwrap_or_default()
                };
                let folders = folders
                    .iter()
                    .map(|f| (f.id.clone(), folder_path_in(&folders, &f.id)))
                    .filter(|(_, trail)| !trail.is_empty())
                    .collect();
                CreateTarget { host: name, index, current: index == current, folders }
            })
            .collect()
    }

    /// Create in `target`: immediately when it is the connected host, otherwise
    /// switch to it and let the create fire once its tree arrives.
    pub fn create_in(&mut self, target: &CreateTarget, kind: CreateKind, folder: Option<Ref>) {
        if target.current {
            self.pending_create = None;
            self.spawn_create(kind, folder);
            return;
        }
        self.pending_create = Some(PendingCreate {
            host: target.host.clone(),
            kind,
            folder,
        });
        self.actions.push(Action::SwitchHost(target.index));
    }

    fn spawn_create(&mut self, kind: CreateKind, folder: Option<Ref>) {
        match kind {
            CreateKind::Session => self.new_session(folder),
            CreateKind::Folder => self.new_folder(folder),
        }
    }

    /// Fire a create that was waiting for another host to connect.
    pub fn fire_pending_create(&mut self) {
        let Some(pending) = self.pending_create.clone() else {
            return;
        };
        if pending.host != self.current_host {
            return;
        }
        self.pending_create = None;
        self.spawn_create(pending.kind, pending.folder);
    }

    pub fn selected_workspace(&self) -> Option<&lmux_proto::Workspace> {
        self.selected.as_ref().and_then(|id| self.workspace(id))
    }

    pub fn pane(&self, id: &str) -> Option<&Pane> {
        self.tree
            .workspaces
            .iter()
            .flat_map(|w| w.panes.iter())
            .find(|p| p.id == id)
    }

    pub fn surface(&self, id: &str) -> Option<&Surface> {
        self.tree
            .workspaces
            .iter()
            .flat_map(|w| w.panes.iter())
            .flat_map(|p| p.surfaces.iter())
            .find(|s| s.id == id)
    }

    /// Active surface of a pane, honouring local tab selection.
    pub fn active_surface(&self, pane: &Pane) -> Option<Ref> {
        if let Some(chosen) = self.pane_active.get(&pane.id) {
            if pane.surfaces.iter().any(|s| &s.id == chosen) {
                return Some(chosen.clone());
            }
        }
        pane.active_surface.clone()
    }

    /// The pane that owns a surface, across all workspaces.
    pub fn pane_of_surface(&self, surface: &str) -> Option<Ref> {
        self.tree
            .workspaces
            .iter()
            .flat_map(|w| w.panes.iter())
            .find(|p| p.surfaces.iter().any(|s| s.id == surface))
            .map(|p| p.id.clone())
    }

    pub fn focus_pane(&mut self, pane: &str) {
        if self.focused_pane().as_deref() == Some(pane) {
            return;
        }
        self.rpc(m::PANE_FOCUS, serde_json::json!({ "pane": pane }));
    }

    pub fn focused_pane(&self) -> Option<Ref> {
        self.selected_workspace().and_then(|w| w.focused_pane.clone())
    }

    pub fn unread_total(&self) -> u32 {
        self.notifications.iter().filter(|n| !n.read).count() as u32
    }

    // --- selection ---------------------------------------------------------

    pub fn select_workspace(&mut self, id: &str) {
        let Some(w) = self.workspace(id) else { return };
        let id = w.id.clone();
        self.activated = true;
        // Looking at a workspace reads its notifications (the daemon does the
        // same on workspace.select), so the bell and panel agree at once.
        for n in self.notifications.iter_mut().filter(|n| n.workspace.as_deref() == Some(id.as_str())) {
            n.read = true;
        }
        if self.selected.as_deref() == Some(id.as_str()) {
            return;
        }
        self.selected = Some(id.clone());
        // Keep a small navigation history for the top-strip arrows.
        self.ws_history.truncate(self.ws_history_pos);
        if self.ws_history.last() != Some(&id) {
            self.ws_history.push(id.clone());
            self.ws_history_pos = self.ws_history.len();
        }
        self.rpc(m::WORKSPACE_SELECT, serde_json::json!({ "workspace": id }));
    }

    pub fn navigate(&mut self, delta: isize) {
        if self.ws_history.is_empty() {
            return;
        }
        let pos = self.ws_history_pos as isize + delta;
        let pos = pos.clamp(0, self.ws_history.len() as isize - 1) as usize;
        if pos == self.ws_history_pos {
            return;
        }
        self.ws_history_pos = pos;
        let id = self.ws_history[pos].clone();
        if self.workspace(&id).is_some() {
            self.selected = Some(id.clone());
            self.rpc(m::WORKSPACE_SELECT, serde_json::json!({ "workspace": id }));
        }
    }

    pub fn can_go_back(&self) -> bool {
        self.ws_history_pos > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.ws_history_pos + 1 < self.ws_history.len()
    }

    /// Workspaces in the order the sidebar shows them: each folder's nested
    /// folders and then its own sessions, depth first, then the top level;
    /// folded folders' sessions are left out. Cmd+1…9 and next/previous
    /// workspace follow this order.
    pub fn sidebar_order(&self) -> Vec<Ref> {
        fn walk(st: &AppState, parent: Option<&str>, out: &mut Vec<Ref>) {
            for f in st.tree.folders.iter().filter(|f| f.parent.as_deref() == parent) {
                // A folded folder's sessions are not on screen, so they get
                // no number, as in cmux.
                if f.collapsed || st.collapsed.contains(&f.id) {
                    continue;
                }
                walk(st, Some(&f.id), out);
                out.extend(
                    st.tree
                        .workspaces
                        .iter()
                        .filter(|w| w.folder.as_deref() == Some(f.id.as_str()))
                        .map(|w| w.id.clone()),
                );
            }
        }
        let mut out = Vec::new();
        walk(self, None, &mut out);
        out.extend(self.tree.workspaces.iter().filter(|w| w.folder.is_none()).map(|w| w.id.clone()));
        // A workspace whose folder is unknown still gets a place.
        let known: std::collections::HashSet<&str> = self.tree.folders.iter().map(|f| f.id.as_str()).collect();
        for w in &self.tree.workspaces {
            let orphan = w.folder.as_deref().is_some_and(|f| !known.contains(f));
            if orphan && !out.contains(&w.id) {
                out.push(w.id.clone());
            }
        }
        out
    }

    /// Select the next (`delta` 1) or previous (-1) workspace in sidebar order,
    /// wrapping around.
    pub fn cycle_workspace(&mut self, delta: isize) {
        let order = self.sidebar_order();
        if order.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|s| order.iter().position(|w| w == s))
            .map(|i| i as isize)
            .unwrap_or(-delta.signum().min(0));
        let next = (at + delta).rem_euclid(order.len() as isize) as usize;
        let id = order[next].clone();
        self.select_workspace(&id);
    }

    /// The focused pane's tabs and the index of the active one.
    fn focused_tabs(&self) -> Option<(Ref, Vec<Ref>, usize)> {
        let pane_id = self.focused_pane()?;
        let pane = self.pane(&pane_id)?;
        let tabs: Vec<Ref> = pane.surfaces.iter().map(|s| s.id.clone()).collect();
        let active = self.active_surface(pane)?;
        let at = tabs.iter().position(|t| *t == active)?;
        Some((pane_id, tabs, at))
    }

    /// Next/previous tab in the focused pane, wrapping around.
    pub fn cycle_tab(&mut self, delta: isize) {
        let Some((pane, tabs, at)) = self.focused_tabs() else { return };
        let next = (at as isize + delta).rem_euclid(tabs.len() as isize) as usize;
        self.select_tab(&pane, &tabs[next]);
    }

    /// Ctrl+1…9: the n-th tab of the focused pane (0-based); 8 means the last.
    pub fn select_tab_index(&mut self, index: usize) {
        let Some((pane, tabs, _)) = self.focused_tabs() else { return };
        let i = if index >= 8 { tabs.len() - 1 } else { index };
        if let Some(t) = tabs.get(i) {
            let t = t.clone();
            self.select_tab(&pane, &t);
        }
    }

    /// Cmd+Opt+arrow: focus the nearest pane in a direction, by the rects the
    /// panes were drawn at last frame. `dir` is (dx, dy), one of them 0.
    pub fn focus_direction(&mut self, dir: (i8, i8)) {
        let Some(ws) = self.selected_workspace() else { return };
        let panes: Vec<Ref> = ws.panes.iter().map(|p| p.id.clone()).collect();
        let Some(cur) = self.focused_pane() else { return };
        let Some(from) = self.pane_rects.get(&cur).copied() else { return };
        let c = from.center();
        let mut best: Option<(f32, Ref)> = None;
        for p in panes.iter().filter(|p| **p != cur) {
            let Some(r) = self.pane_rects.get(p) else { continue };
            // Strictly beyond the current pane's edge in that direction.
            let ahead = match dir {
                (1, _) => r.min.x >= from.max.x - 1.0,
                (-1, _) => r.max.x <= from.min.x + 1.0,
                (_, 1) => r.min.y >= from.max.y - 1.0,
                _ => r.max.y <= from.min.y + 1.0,
            };
            if !ahead {
                continue;
            }
            let rc = r.center();
            let (along, across) = if dir.0 != 0 {
                ((rc.x - c.x).abs(), (rc.y - c.y).abs())
            } else {
                ((rc.y - c.y).abs(), (rc.x - c.x).abs())
            };
            let score = along + 2.0 * across;
            if best.as_ref().is_none_or(|(b, _)| score < *b) {
                best = Some((score, p.clone()));
            }
        }
        if let Some((_, p)) = best {
            self.focus_pane(&p);
        }
    }

    /// Cmd+Shift+U: the newest unread notification's workspace, marked read.
    pub fn jump_to_unread(&mut self) -> bool {
        let Some(n) = self
            .notifications
            .iter()
            .filter(|n| !n.read && n.workspace.is_some())
            .max_by_key(|n| n.created_at)
            .cloned()
        else {
            return false;
        };
        self.rpc(m::NOTIFY_MARK_READ, serde_json::json!({ "id": n.id }));
        for item in self.notifications.iter_mut().filter(|i| i.id == n.id) {
            item.read = true;
        }
        if let Some(ws) = n.workspace {
            self.select_workspace(&ws);
        }
        true
    }

    /// Open `url` in a new browser tab in a workspace's focused pane, and
    /// show that workspace. The browser runs on the workspace's host, so
    /// `http://localhost:PORT` reaches a server running there.
    pub fn open_in_browser_tab(&mut self, ws: &str, url: &str) {
        self.select_workspace(ws);
        let Some(pane) = self
            .workspace(ws)
            .and_then(|w| w.focused_pane.clone().or_else(|| w.panes.first().map(|p| p.id.clone())))
        else {
            return;
        };
        self.pane_active.remove(&pane);
        self.rpc(m::SURFACE_NEW, serde_json::json!({ "pane": pane, "type": "browser", "url": url }));
    }

    /// Start editing the selected (or given) workspace's title in place.
    pub fn begin_ws_rename(&mut self, ws: Option<&str>) {
        let id = ws.map(str::to_owned).or_else(|| self.selected.clone());
        let Some(id) = id else { return };
        let Some(title) = self.workspace(&id).map(|w| w.title.clone()) else { return };
        self.ws_rename = Some(FolderRename { folder: id, text: title, focus_requested: false });
    }

    /// Cmd+Shift+Enter: show the focused pane alone, or go back to the split.
    pub fn toggle_zoom(&mut self) {
        self.zoomed_pane = match (&self.zoomed_pane, self.focused_pane()) {
            (Some(_), _) => None,
            (None, Some(p)) if self.selected_workspace().is_some_and(|w| w.panes.len() > 1) => Some(p),
            _ => None,
        };
    }

    /// Cmd+= / Cmd+- / Cmd+0. Sizes stay between 8 and 32 points.
    pub fn zoom_terminal(&mut self, delta: f32) {
        self.term_font = if delta == 0.0 {
            crate::theme::FS_TERM
        } else {
            (self.term_font + delta).clamp(8.0, 32.0)
        };
    }

    pub fn select_tab(&mut self, pane: &str, surface: &str) {
        self.pane_active.insert(pane.to_owned(), surface.to_owned());
    }

    /// Start editing a chip's title in place.
    pub fn begin_rename(&mut self, surface: &str, current: &str) {
        self.renaming = Some(Rename {
            surface: surface.to_owned(),
            text: current.to_owned(),
            focus_requested: false,
        });
    }

    /// Close one surface and let the selection follow the close: the chip to the
    /// right, or the one to the left when the closed chip was last. Closing a
    /// background chip leaves the active one alone.
    pub fn close_surface(&mut self, pane_id: &str, surface_id: &str) {
        let before = self.pane_active.get(pane_id).cloned();
        let surfaces: Vec<Ref> = self
            .pane(pane_id)
            .map(|p| p.surfaces.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default();
        let active = self.pane(pane_id).and_then(|p| self.active_surface(p));

        if active.as_deref() == Some(surface_id) {
            if let Some(index) = surfaces.iter().position(|s| s == surface_id) {
                let neighbour = surfaces
                    .get(index + 1)
                    .or_else(|| index.checked_sub(1).and_then(|i| surfaces.get(i)))
                    .cloned();
                match neighbour {
                    Some(next) => {
                        self.pane_active.insert(pane_id.to_owned(), next);
                    }
                    None => {
                        self.pane_active.remove(pane_id);
                    }
                }
            }
        }
        // A chip being renamed and closed should not leave an editor behind.
        if self.renaming.as_ref().is_some_and(|r| r.surface == surface_id) {
            self.renaming = None;
        }
        // Ask the daemon to close only if nothing is running there; a busy
        // pane comes back as a question (see `confirm_close`).
        let id = self.rpc_with_id(m::SURFACE_CLOSE, serde_json::json!({ "surface": surface_id, "if_idle": true }));
        self.pending_closes.insert(
            id,
            CloseTarget::Surface(pane_id.to_owned(), surface_id.to_owned(), before),
        );
    }

    /// Close a workspace, asking first if a program is running in it.
    pub fn close_workspace(&mut self, ws: &str) {
        let id = self.rpc_with_id(m::WORKSPACE_CLOSE, serde_json::json!({ "workspace": ws, "if_idle": true }));
        self.pending_closes.insert(id, CloseTarget::Workspace(ws.to_owned()));
    }

    /// The user answered the "still running" question: close anyway, or not.
    pub fn answer_close(&mut self, close: bool) {
        let Some(q) = self.confirm_close.take() else { return };
        if !close {
            return;
        }
        match q.target {
            CloseTarget::Surface(pane, surface, _) => {
                // The selection moved with the first attempt; move it again,
                // as the tab may have been reselected meanwhile.
                self.close_surface_now(&pane, &surface);
            }
            CloseTarget::Workspace(ws) => {
                self.rpc(m::WORKSPACE_CLOSE, serde_json::json!({ "workspace": ws }));
            }
        }
    }

    fn close_surface_now(&mut self, pane_id: &str, surface_id: &str) {
        let surfaces: Vec<Ref> = self
            .pane(pane_id)
            .map(|p| p.surfaces.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default();
        if self.pane(pane_id).and_then(|p| self.active_surface(p)).as_deref() == Some(surface_id) {
            if let Some(index) = surfaces.iter().position(|s| s == surface_id) {
                let next = surfaces.get(index + 1).or_else(|| index.checked_sub(1).and_then(|i| surfaces.get(i)));
                match next.cloned() {
                    Some(n) => {
                        self.pane_active.insert(pane_id.to_owned(), n);
                    }
                    None => {
                        self.pane_active.remove(pane_id);
                    }
                }
            }
        }
        self.rpc(m::SURFACE_CLOSE, serde_json::json!({ "surface": surface_id }));
    }

    /// `Cmd+W`: close the focused pane's active chip.
    pub fn close_active_chip(&mut self) {
        let Some(pane_id) = self.focused_pane() else { return };
        let Some(surface) = self.pane(&pane_id).and_then(|p| self.active_surface(p)) else {
            return;
        };
        self.close_surface(&pane_id, &surface);
    }

    /// `+` / `Cmd+T`: a new terminal chip in a pane.
    pub fn new_terminal_chip(&mut self, pane: &str) {
        self.clear_tab_override(pane);
        self.rpc(m::SURFACE_NEW, serde_json::json!({ "pane": pane, "type": "terminal" }));
    }

    /// The globe / `Cmd+Shift+B`: a new browser chip in a pane. It opens blank
    /// with the address bar focused, so the user can type straight away.
    pub fn new_browser_chip(&mut self, pane: &str) {
        self.clear_tab_override(pane);
        self.browser_request(
            m::SURFACE_NEW,
            serde_json::json!({ "pane": pane, "type": "browser", "url": "about:blank" }),
            BrowserReply::FocusAddressBar,
        );
    }

    /// "New Terminal/Browser Tab to Right": a new chip right after `after`.
    pub fn new_chip_after(&mut self, pane: &str, after: &str, browser: bool) {
        self.clear_tab_override(pane);
        if browser {
            self.browser_request(
                m::SURFACE_NEW,
                serde_json::json!({ "pane": pane, "type": "browser", "url": "about:blank", "after": after }),
                BrowserReply::FocusAddressBar,
            );
        } else {
            self.rpc(m::SURFACE_NEW, serde_json::json!({ "pane": pane, "type": "terminal", "after": after }));
        }
    }

    /// The pane beside `pane` on the left (`dx` < 0) or right, as laid out on
    /// screen: the nearest one that shares some height with it.
    pub fn pane_beside(&self, pane: &str, dx: i8) -> Option<Ref> {
        let ws = self.selected_workspace()?;
        let me = *self.pane_rects.get(pane)?;
        ws.panes
            .iter()
            .filter(|p| p.id != pane)
            .filter_map(|p| self.pane_rects.get(&p.id).map(|r| (p.id.clone(), *r)))
            .filter(|(_, r)| r.max.y > me.min.y + 1.0 && r.min.y < me.max.y - 1.0)
            .filter(|(_, r)| if dx < 0 { r.max.x <= me.min.x + 1.0 } else { r.min.x >= me.max.x - 1.0 })
            .min_by(|a, b| {
                let d = |r: &egui::Rect| if dx < 0 { me.min.x - r.max.x } else { r.min.x - me.max.x };
                d(&a.1).total_cmp(&d(&b.1))
            })
            .map(|(id, _)| id)
    }

    /// A browser chip at `url` in a pane (middle- or Cmd-click on a link).
    pub fn open_browser_tab(&mut self, pane: &str, url: &str) {
        self.clear_tab_override(pane);
        self.rpc(m::SURFACE_NEW, serde_json::json!({ "pane": pane, "type": "browser", "url": url }));
    }

    /// The focused pane's active surface, when it is a browser.
    pub fn focused_browser(&self) -> Option<Ref> {
        let pane = self.focused_pane()?;
        let sid = self.pane(&pane).and_then(|p| self.active_surface(p))?;
        matches!(self.surface(&sid).map(|s| &s.kind), Some(SurfaceKind::Browser { .. }))
            .then_some(sid)
    }

    /// Cmd+L: edit the address, with all of it selected.
    pub fn focus_address_bar(&mut self, surface: &str) {
        let b = self.ensure_browser(surface, "");
        if b.url_draft.is_none() {
            b.url_draft = Some(if b.url == "about:blank" { String::new() } else { b.url.clone() });
        }
        b.focus_url = true;
    }

    /// Send a browser request whose reply the app acts on.
    pub fn browser_request(&mut self, method: &str, params: serde_json::Value, reply: BrowserReply) {
        let id = self.rpc_with_id(method, params);
        self.browser_replies.insert(id, reply);
    }

    fn on_browser_reply(&mut self, reply: BrowserReply, result: Option<&serde_json::Value>) {
        let Some(result) = result else { return };
        match reply {
            BrowserReply::FocusAddressBar => {
                if let Some(sid) = result.get("surface").and_then(|v| v.as_str()) {
                    self.ensure_browser(sid, "about:blank");
                    self.focus_address_bar(sid);
                }
            }
            BrowserReply::Copy => {
                if let Some(text) = result.get("value").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        self.clipboard_out = Some(text.to_owned());
                    }
                }
            }
            BrowserReply::OpenLink { pane } => {
                if let Some(href) = result.get("href").and_then(|v| v.as_str()) {
                    self.open_browser_tab(&pane, href);
                }
            }
        }
    }

    /// A new surface becomes the pane's active one, so drop any local tab
    /// selection; otherwise the freshly opened chip would stay unselected.
    fn clear_tab_override(&mut self, pane: &str) {
        self.pane_active.remove(pane);
    }

    // --- drag and drop -----------------------------------------------------

    /// Begin dragging a chip, a pane header, or a sidebar row. `source_pane` is
    /// empty for a sidebar row, which belongs to no pane.
    pub fn begin_tab_drag(
        &mut self,
        item: DragItem,
        source_pane: &str,
        grab: egui::Vec2,
        size: egui::Vec2,
        pointer: egui::Pos2,
    ) {
        self.tab_drag = Some(TabDrag {
            item,
            source_pane: source_pane.to_owned(),
            grab,
            size,
            pointer,
        });
        self.tab_drop = None;
    }

    /// Issue the RPC a resolved [`DropTarget`] implies. The tree is not patched
    /// locally: the daemon rebroadcasts it and the UI renders from that.
    pub fn apply_drop(&mut self, target: DropTarget) {
        let Some(drag) = self.tab_drag.as_ref() else { return };
        let source = drag.source_pane.clone();
        match target {
            DropTarget::Move { pane, index } => {
                let DragItem::Surface(surface) = &drag.item else { return };
                let mut params = serde_json::json!({ "surface": surface, "pane": pane.clone() });
                if let Some(index) = index {
                    params["index"] = serde_json::json!(index);
                }
                self.rpc(m::SURFACE_MOVE, params);
                // The daemon makes the moved surface active; drop any local tab
                // selection that would hide it.
                self.pane_active.remove(&source);
                self.pane_active.remove(&pane);
            }
            DropTarget::Split { pane, side } => {
                let DragItem::Surface(surface) = &drag.item else { return };
                self.rpc(
                    m::SURFACE_SPLIT,
                    serde_json::json!({ "surface": surface, "side": side, "pane": pane }),
                );
                // The daemon makes the moved surface active in the new pane; drop
                // any local tab selection that would hide it.
                self.pane_active.remove(&source);
                self.pane_active.remove(&pane);
            }
            DropTarget::Swap { other } => {
                let DragItem::Pane(pane) = &drag.item else { return };
                self.rpc(m::PANE_SWAP, serde_json::json!({ "pane": pane, "other": other }));
            }
            DropTarget::Sidebar { into, slot, .. } => {
                // A dropped row inside a collapsed folder would be invisible, so
                // reveal the folder that just received it.
                let into = match into {
                    Some(folder) => {
                        self.collapsed.remove(&folder);
                        Some(folder)
                    }
                    None => None,
                };
                match &drag.item {
                    DragItem::Workspace(ws) => {
                        let index = workspace_move_index(&self.tree, ws, into.as_deref(), slot);
                        let mut params = serde_json::json!({ "workspace": ws, "folder": into });
                        if let Some(index) = index {
                            params["index"] = serde_json::json!(index);
                        }
                        self.rpc(m::WORKSPACE_MOVE, params);
                    }
                    DragItem::Folder(folder) => {
                        let index = folder_move_index(&self.tree, folder, into.as_deref(), slot);
                        let mut params = serde_json::json!({ "folder": folder, "parent": into });
                        if let Some(index) = index {
                            params["index"] = serde_json::json!(index);
                        }
                        self.rpc(m::FOLDER_MOVE, params);
                    }
                    _ => {}
                }
            }
        }
    }

    /// End a drag without acting on it.
    pub fn cancel_tab_drag(&mut self) {
        self.tab_drag = None;
        self.tab_drop = None;
    }

    // --- server messages ---------------------------------------------------

    pub fn handle_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Welcome { server_version, host, .. } => {
                self.host = host;
                self.server_version = server_version;
            }
            ServerMsg::Tree { tree } => self.apply_tree(tree),
            ServerMsg::TermSnapshot { surface, cols, rows, data } => {
                let tv = self
                    .terms
                    .entry(surface.clone())
                    .or_insert_with(|| TermView::new(cols, rows));
                tv.apply_snapshot(cols, rows, &data);
            }
            ServerMsg::TermExited { surface } => {
                if let Some(t) = self.terms.get_mut(&surface) {
                    t.exited = true;
                }
            }
            ServerMsg::BrowserMeta { surface, url, title, loading, can_go_back, can_go_forward } => {
                let b = self
                    .browsers
                    .entry(surface.clone())
                    .or_insert_with(|| BrowserView::new(url.clone()));
                b.url = url;
                b.title = title;
                b.loading = loading;
                b.can_back = can_go_back;
                b.can_fwd = can_go_forward;
            }
            ServerMsg::Notification { notification } => {
                if !self.notifications.iter().any(|n| n.id == notification.id) {
                    self.os_queue.push(notification.clone());
                    self.notifications.push(notification);
                }
            }
            ServerMsg::Log { .. } => {}
            ServerMsg::AppBrowserCall { id, surface, op } => {
                self.app_calls.push(AppCall { id, surface, op, since: Instant::now() });
            }
            ServerMsg::RpcResult { response } => {
                if let Some(reply) = self.browser_replies.remove(&response.id) {
                    self.on_browser_reply(reply, response.result.as_ref());
                }
                // An uploaded image: paste the path it was saved at on the host.
                if let Some(surface) = self.pending_uploads.remove(&response.id) {
                    if let Some(path) = response.result.as_ref().and_then(|r| r.get("path")).and_then(|p| p.as_str()) {
                        let path = path.to_owned();
                        self.paste_text(&surface, &path);
                    }
                }
                let close = self.pending_closes.remove(&response.id);
                match (response.error, close) {
                    (Some(err), Some(target)) if err.code == lmux_proto::RpcErrorCode::Busy => {
                        // Put the tab selection back: nothing was closed.
                        if let CloseTarget::Surface(pane, _, before) = &target {
                            match before {
                                Some(b) => {
                                    self.pane_active.insert(pane.clone(), b.clone());
                                }
                                None => {
                                    self.pane_active.remove(pane);
                                }
                            }
                        }
                        self.confirm_close = Some(ConfirmClose { target, program: err.message });
                    }
                    (Some(err), _) => {
                        self.banner = Some((format!("{}: {}", err.code_str(), err.message), true));
                    }
                    _ => {}
                }
                if self.folder_rename_pending == Some(response.id) {
                    self.folder_rename_pending = None;
                    let folder = response
                        .result
                        .as_ref()
                        .and_then(|r| r.get("folder"))
                        .and_then(|f| f.as_str())
                        .map(str::to_owned);
                    if let Some(folder) = folder {
                        self.begin_folder_rename(&folder, "New Folder");
                    }
                }
            }
        }
    }

    pub fn apply_tree(&mut self, mut tree: Tree) {
        for ws in &mut tree.workspaces {
            let bare = crate::util::strip_title_icon(&ws.title);
            if bare.len() != ws.title.len() {
                ws.title = bare.to_owned();
            }
        }
        // Remember this host's folders, so the create menu can offer them even
        // while another host is on screen.
        if !self.current_host.is_empty() {
            self.host_folders
                .insert(self.current_host.clone(), tree.folders.clone());
        }
        // Keep the locally chosen tab where the pane still offers it.
        self.pane_active.retain(|pane, surface| {
            tree.workspaces
                .iter()
                .flat_map(|w| w.panes.iter())
                .any(|p| &p.id == pane && p.surfaces.iter().any(|s| &s.id == surface))
        });

        self.tree = tree;

        // A folder being renamed that no longer exists must not hold an editor.
        let stale_rename = self
            .folder_rename
            .as_ref()
            .is_some_and(|r| self.folder(&r.folder).is_none());
        if stale_rename {
            self.folder_rename = None;
        }

        if self.selected.as_ref().is_none_or(|id| self.workspace(id).is_none()) {
            self.selected = self.tree.selected_workspace.clone();
        }
        if let Some(sel) = self.selected.clone() {
            if !self.tree.workspaces.iter().any(|w| w.id == sel) {
                self.selected = self.tree.selected_workspace.clone();
            }
        }
        if self.selected.is_none() {
            self.selected = self.tree.workspaces.first().map(|w| w.id.clone());
        }

        // Drop terminal/browser state for surfaces that no longer exist.
        let live: HashSet<Ref> = self
            .tree
            .workspaces
            .iter()
            .flat_map(|w| w.panes.iter())
            .flat_map(|p| p.surfaces.iter())
            .map(|s| s.id.clone())
            .collect();
        self.terms.retain(|k, v| {
            let keep = live.contains(k);
            if !keep {
                v.subscribed = false;
            }
            keep
        });
        self.browsers.retain(|k, v| {
            let keep = live.contains(k);
            if !keep {
                v.subscribed = false;
            }
            keep
        });
        // A local PTY exists only while its surface does. Dropping it hangs up
        // the tmux client, and the view session goes with it.
        self.local_ptys.retain(|k, _| live.contains(k));
        self.local_wanted.retain(|k| live.contains(k));
        self.local_retry.retain(|k, _| live.contains(k));

        // A create aimed at this host can go out now.
        self.fire_pending_create();
    }

    pub fn handle_frame(&mut self, buf: &[u8]) {
        match wire::Frame::decode(buf) {
            Ok(wire::Frame::TermOutput { surface, data }) => {
                if let Some(t) = self.terms.get_mut(surface) {
                    t.feed(data);
                    t.exited = false;
                }
            }
            Ok(wire::Frame::BrowserFrame { surface, seq, jpeg, .. }) => {
                // Decode on the worker thread; the result is picked up by
                // `poll_decoded` and acked after its texture is uploaded.
                let wake = self.wake.clone();
                let dec = self.decoder.get_or_insert_with(|| {
                    let d = crate::decode::Decoder::new();
                    if let Some(ctx) = wake {
                        d.set_wake(ctx);
                    }
                    d
                });
                dec.submit(surface.to_owned(), seq, jpeg.to_vec());
            }
            Err(e) => eprintln!("[frame] decode failed: {e:?}"),
        }
    }

    /// Collect frames finished by the decode worker and queue an ack for the
    /// one that replaced a surface's image. Called once per pump.
    pub fn poll_decoded(&mut self) {
        let Some(dec) = &self.decoder else { return };
        for frame in dec.drain() {
            let b = self
                .browsers
                .entry(frame.surface.clone())
                .or_insert_with(|| BrowserView::new(String::new()));
            b.frame_size = (frame.width, frame.height);
            b.seq = frame.seq;
            b.pending_image = Some(frame.image);
            b.have_frame = true;
            b.pending_ack = Some(frame.seq);
        }
    }

    // --- visibility pass ---------------------------------------------------

    /// Called after the UI has rendered; turns this frame's visible set into
    /// subscribe/unsubscribe/resize traffic.
    pub fn sync_subscriptions(&mut self, connected: bool) {
        let visible = std::mem::take(&mut self.visible);
        if !connected {
            // Messages sent while down are dropped, so never claim to be
            // subscribed; everything re-subscribes once the link is back.
            for t in self.terms.values_mut() {
                t.subscribed = false;
            }
            for b in self.browsers.values_mut() {
                b.subscribed = false;
                b.sent_size = None;
            }
            return;
        }

        // Terminals: streamed from the daemon, or attached by this process.
        if self.local_mode {
            self.sync_local_terms(&visible);
        } else {
            self.sync_streamed_terms(&visible);
        }

        // Browsers.
        let ids: Vec<Ref> = self.browsers.keys().cloned().collect();
        for id in &ids {
            let visible_now = visible.contains(id);
            let Some(b) = self.browsers.get_mut(id) else { continue };
            b.wants_visible = visible_now;
            if visible_now && !b.subscribed {
                b.subscribed = true;
                self.pending.push(ClientMsg::Subscribe { surface: id.clone(), history: None });
            } else if !visible_now && b.subscribed {
                b.subscribed = false;
                b.sent_size = None;
                self.pending.push(ClientMsg::Unsubscribe { surface: id.clone() });
            }
        }
    }

    /// Terminals the daemon streams: subscribe while visible, unsubscribe while
    /// hidden, and let the daemon resize their tmux windows.
    fn sync_streamed_terms(&mut self, visible: &HashSet<Ref>) {
        // Views hidden for a while go: each holds a screen and its history,
        // and every terminal ever shown used to stay in memory.
        let now = Instant::now();
        self.terms.retain(|id, t| {
            visible.contains(id) || t.hidden_since.is_none_or(|h| now.duration_since(h) < HIDDEN_VIEW_TTL)
        });
        let ids: Vec<Ref> = self.terms.keys().cloned().collect();
        for id in &ids {
            let visible_now = visible.contains(id);
            let Some(t) = self.terms.get_mut(id) else { continue };
            t.wants_visible = visible_now;
            t.hidden_since = if visible_now { None } else { t.hidden_since.or(Some(now)) };
            if visible_now && !t.subscribed {
                t.subscribed = true;
                self.pending.push(ClientMsg::Subscribe { surface: id.clone(), history: None });
            } else if !visible_now && t.subscribed {
                t.subscribed = false;
                self.pending.push(ClientMsg::Unsubscribe { surface: id.clone() });
            }
            if let (true, Some(size)) = (t.subscribed, self.desired_sizes.get(id).copied()) {
                if (t.cols(), t.rows()) != size && t.size() != crate::term::TermSize::new(size.0 as usize, size.1 as usize) {
                    t.resize(size.0, size.1);
                    self.pending.push(ClientMsg::TermResize {
                        surface: id.clone(),
                        cols: size.0,
                        rows: size.1,
                    });
                }
            }
        }
    }

    /// Terminals on this machine. The daemon gets no subscribe, no unsubscribe
    /// and no resize for these: the PTY is the pane's only attachment, and with
    /// `window-size latest` set on the window it is the PTY that decides the
    /// size. A PTY is spawned the first time its surface is visible and kept
    /// until the surface leaves the tree, so a hidden tab comes back instantly.
    fn sync_local_terms(&mut self, visible: &HashSet<Ref>) {
        let ids: Vec<Ref> = self.terms.keys().cloned().collect();
        let now = Instant::now();
        for id in &ids {
            let visible_now = visible.contains(id);
            let pane = self.surface_tmux_pane(id);
            if let Some(t) = self.terms.get_mut(id) {
                t.wants_visible = visible_now;
                t.subscribed = false;
            }
            if visible_now {
                self.local_wanted.insert(id.clone());
            }
            // Without a tmux pane id there is nothing to attach to yet; the next
            // tree carries it.
            let Some(pane) = pane else { continue };

            // A PTY attached to a window that is no longer this surface's is
            // replaced; a PTY whose tmux client exited is retried after a pause.
            let stale = self
                .local_ptys
                .get(id)
                .is_some_and(|p| p.tmux_pane.as_deref() != Some(pane.as_str()));
            if stale {
                self.local_ptys.remove(id);
            } else if self.local_ptys.get_mut(id).is_some_and(|p| !p.is_alive()) {
                self.local_ptys.remove(id);
                self.local_retry.insert(id.clone(), now + LOCAL_PTY_RETRY);
            }

            if !self.local_ptys.contains_key(id)
                && self.local_wanted.contains(id)
                && self.local_retry.get(id).is_none_or(|t| *t <= now)
            {
                let (cols, rows) = self.desired_sizes.get(id).copied().unwrap_or((80, 24));
                match LocalPty::spawn(id, &pane, cols, rows, self.wake.clone()) {
                    Ok(pty) => {
                        self.local_retry.remove(id);
                        self.local_ptys.insert(id.clone(), pty);
                    }
                    Err(e) => {
                        eprintln!("[localpty] {id}: {e}");
                        self.local_retry.insert(id.clone(), now + LOCAL_PTY_RETRY);
                    }
                }
            }

            // The painter's size drives both: the PTY tells tmux (with
            // `window-size latest` on the window) and the local grid is resized
            // to match, so the pane is never a frame out of step with its PTY.
            let size = self.desired_sizes.get(id).copied().unwrap_or((80, 24));
            if let Some(pty) = self.local_ptys.get_mut(id) {
                pty.resize(size.0, size.1);
            }
            if let Some(t) = self.terms.get_mut(id) {
                t.resize(size.0, size.1);
            }
        }
    }

    /// The tmux pane backing a terminal surface, if the tree names one yet.
    fn surface_tmux_pane(&self, surface: &str) -> Option<String> {
        match self.surface(surface).map(|s| &s.kind) {
            Some(SurfaceKind::Terminal { tmux_pane, .. }) => tmux_pane.clone(),
            _ => None,
        }
    }

    /// egui context the PTY reader threads wake when output arrives.
    pub fn set_wake(&mut self, ctx: egui::Context) {
        self.wake = Some(ctx);
    }

    /// Record whether the connected host is this machine. Leaving the local host
    /// closes every local attachment; the surfaces belong to its tree.
    pub fn set_local_mode(&mut self, local: bool) {
        if self.local_mode == local {
            return;
        }
        self.local_mode = local;
        if !local {
            self.local_ptys.clear();
            self.local_wanted.clear();
            self.local_retry.clear();
        }
    }

    /// True while this surface is a terminal served by a PTY in this process.
    pub fn local_terminal(&self, surface: &str) -> bool {
        self.local_mode && self.surface_tmux_pane(surface).is_some()
    }

    /// Feed everything the local PTYs produced since the last frame into their
    /// terminal views (the same grid the daemon streams into for remote panes).
    pub fn pump_local_ptys(&mut self) {
        let ids: Vec<Ref> = self.local_ptys.keys().cloned().collect();
        for id in ids {
            let chunks = self.local_ptys.get_mut(&id).map(|p| p.drain()).unwrap_or_default();
            if chunks.is_empty() {
                continue;
            }
            let tv = self.terms.entry(id).or_insert_with(|| TermView::new(80, 24));
            for chunk in &chunks {
                tv.feed(chunk);
            }
            tv.exited = false;
        }
    }

    /// Write key bytes (or paste) straight to a local PTY. `false` when this
    /// surface has no PTY, so the caller can fall back to the daemon.
    pub fn write_local(&mut self, surface: &str, bytes: &[u8]) -> bool {
        let Some(pty) = self.local_ptys.get_mut(surface) else { return false };
        if let Err(e) = pty.write(bytes) {
            eprintln!("[localpty] write {surface}: {e}");
        }
        true
    }

    /// Send input bytes to a terminal: a local pane's PTY when this process
    /// owns one, otherwise the daemon.
    /// Cmd+V found no text: read an image off the clipboard in the
    /// background. `poll_image_pastes` pastes it as a file path, after
    /// uploading it to the host for a remote pane, since a terminal only
    /// carries text and Claude Code attaches an image given its path.
    pub fn request_image_paste(&mut self, surface: &str) {
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::clip::clipboard_png());
            if let Some(c) = wake {
                c.request_repaint();
            }
        });
        self.image_reads.push((surface.to_owned(), rx));
    }

    /// Finish image pastes whose clipboard read is done.
    pub fn poll_image_pastes(&mut self) {
        let mut done: Vec<(Ref, std::path::PathBuf)> = Vec::new();
        self.image_reads.retain(|(surface, rx)| match rx.try_recv() {
            Ok(Some(path)) => {
                done.push((surface.clone(), path));
                false
            }
            Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
        });
        for (surface, path) in done {
            if self.local_mode {
                // This Mac: the file is already where the program can read it.
                self.paste_text(&surface, &path.to_string_lossy());
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            self.upload_and_paste(&surface, "pasted-image.png", &bytes);
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Save `bytes` on the host as a file and, once it answers, paste the
    /// file's path into the terminal.
    pub fn upload_and_paste(&mut self, surface: &str, name: &str, bytes: &[u8]) {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let id = self.rpc_with_id(m::FILE_PUT, serde_json::json!({ "name": name, "data": data }));
        self.pending_uploads.insert(id, surface.to_owned());
    }

    /// Paste text into a terminal as the keyboard would (bracketed when the
    /// program asked for it).
    pub fn paste_text(&mut self, surface: &str, text: &str) {
        let bracketed = self.term_mode(surface).contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE);
        let bytes = crate::termio::paste_bytes(text, bracketed);
        self.send_term_input(surface, &bytes);
    }

    /// Text terminals copied with OSC 52 since the last call: the app puts
    /// it on the system clipboard. Remote panes included, since the escape
    /// travels with their output.
    pub fn take_clipboard_requests(&mut self) -> Vec<String> {
        self.terms.values_mut().flat_map(|t| t.take_clipboard()).collect()
    }

    /// Scroll a terminal the daemon streams. Its local history is a replay of
    /// the stream, which goes wrong when the size changed mid-stream or a
    /// program redrew its whole frame; so leaving the bottom first fetches
    /// tmux's own history (a snapshot with it) and scrolls once that is in.
    pub fn scroll_streamed(&mut self, surface: &str, lines: i32) {
        let Some(tv) = self.terms.get_mut(surface) else { return };
        if let Some(pending) = tv.history_scroll.as_mut() {
            *pending = (*pending + lines).max(0);
            return;
        }
        if lines > 0 && tv.display_offset() == 0 && tv.subscribed {
            tv.history_scroll = Some(lines);
            self.pending.push(ClientMsg::Subscribe { surface: surface.to_owned(), history: Some(STREAMED_HISTORY) });
            return;
        }
        tv.scroll(lines);
    }

    pub fn send_term_input(&mut self, surface: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let wrote = self.local_terminal(surface) && self.write_local(surface, bytes);
        if !wrote {
            let data = String::from_utf8_lossy(bytes).into_owned();
            self.pending.push(ClientMsg::TermInput { surface: surface.to_owned(), data });
        }
    }

    /// Tell terminals that asked for focus events (mode 1004, which Claude Code
    /// uses) whether they have the keyboard: only the focused pane's terminal
    /// does, and only while the window is focused. A local pane's mode is its
    /// tmux client's, so the report goes to that client, which passes it on
    /// to the pane; a streamed pane's mode is the program's own, and the
    /// report goes straight to it. Hidden terminals are left alone: their
    /// modes may be stale, and a report a program did not ask for would be
    /// typed into it.
    pub fn sync_focus_reports(&mut self, window_focused: bool) {
        let focused = if window_focused {
            self.focused_pane()
                .and_then(|p| self.pane(&p).and_then(|p| self.active_surface(p)))
        } else {
            None
        };
        let ids: Vec<Ref> = self.terms.keys().cloned().collect();
        for id in ids {
            let local = self.local_terminal(&id);
            let live = if local {
                self.local_ptys.contains_key(&id)
            } else {
                self.terms.get(&id).is_some_and(|t| t.subscribed)
            };
            if !live {
                // A reattached tmux client starts out focused.
                if local {
                    self.focus_reported.remove(&id);
                }
                continue;
            }
            if !self.term_mode(&id).contains(alacritty_terminal::term::TermMode::FOCUS_IN_OUT) {
                continue;
            }
            let want = focused.as_deref() == Some(id.as_str());
            if self.focus_reported.get(&id).copied().unwrap_or(true) == want {
                continue;
            }
            self.focus_reported.insert(id.clone(), want);
            let bytes: &[u8] = if want { b"\x1b[I" } else { b"\x1b[O" };
            if local {
                self.write_local(&id, bytes);
            } else {
                self.pending.push(ClientMsg::TermInput {
                    surface: id.clone(),
                    data: String::from_utf8_lossy(bytes).into_owned(),
                });
            }
        }
        self.focus_reported.retain(|id, _| self.terms.contains_key(id));
    }

    /// The terminal modes the program in `surface` has set (empty if unknown).
    pub fn term_mode(&self, surface: &str) -> alacritty_terminal::term::TermMode {
        self.terms.get(surface).map(|t| t.mode()).unwrap_or_else(alacritty_terminal::term::TermMode::empty)
    }

    /// Unsubscribe everything (window minimised/occluded): hidden panes must
    /// cost the daemon nothing.
    pub fn stop_all(&mut self) {
        for (id, t) in self.terms.iter_mut() {
            if t.subscribed {
                t.subscribed = false;
                self.pending.push(ClientMsg::Unsubscribe { surface: id.clone() });
            }
        }
        for (id, b) in self.browsers.iter_mut() {
            if b.subscribed {
                b.subscribed = false;
                b.sent_size = None;
                self.pending.push(ClientMsg::Unsubscribe { surface: id.clone() });
            }
        }
    }

    /// Called when the connection drops: forget what we thought was subscribed.
    pub fn reset_subscriptions(&mut self) {
        for t in self.terms.values_mut() {
            t.subscribed = false;
            t.history_scroll = None;
        }
        for b in self.browsers.values_mut() {
            b.subscribed = false;
            b.sent_size = None;
        }
    }

    pub fn ensure_term(&mut self, surface: &str) -> &mut TermView {
        self.terms.entry(surface.to_owned()).or_insert_with(|| TermView::new(80, 24))
    }

    /// A browser the app draws as a native WebKit view: one its daemon made
    /// for the app (`lmux server --app-browser`).
    pub fn in_app_browser(&self, surface: &str) -> bool {
        matches!(self.surface(surface).map(|s| &s.kind), Some(SurfaceKind::Browser { in_app: true, .. }))
    }

    /// The page a browser surface is at, as the daemon last heard.
    pub fn browser_url(&self, surface: &str) -> Option<String> {
        match &self.surface(surface)?.kind {
            SurfaceKind::Browser { url, .. } => Some(url.clone()),
            _ => None,
        }
    }

    pub fn ensure_browser(&mut self, surface: &str, url: &str) -> &mut BrowserView {
        self.browsers
            .entry(surface.to_owned())
            .or_insert_with(|| BrowserView::new(url.to_owned()))
    }

    /// Walk a workspace layout in visual order.
    pub fn layout_panes(layout: &Layout) -> Vec<Ref> {
        crate::layout::pane_order(layout)
    }

    pub fn surface_is_terminal(&self, id: &str) -> bool {
        matches!(self.surface(id).map(|s| &s.kind), Some(SurfaceKind::Terminal { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmux_proto::{AgentState, Layout, Pane, Progress, SplitDirection, Surface, SurfaceKind, Tree, Workspace};
    /// Scrolling back a streamed terminal fetches tmux's history first, then
    /// scrolls into it once the snapshot carrying it is in.
    #[test]
    fn streamed_scrollback_comes_from_tmux() {
        let mut s = AppState::new();
        let tv = s.terms.entry("surface:1".into()).or_insert_with(|| TermView::new(20, 3));
        tv.subscribed = true;
        s.scroll_streamed("surface:1", 2);
        assert!(s.pending.iter().any(|m| matches!(m,
            ClientMsg::Subscribe { surface, history: Some(_) } if surface == "surface:1")));
        assert_eq!(s.terms["surface:1"].display_offset(), 0, "nothing to scroll into yet");
        // More wheel while waiting adds up.
        s.scroll_streamed("surface:1", 1);
        let history: String = (1..=10).map(|i| format!("{i}\r\n")).collect();
        s.terms.get_mut("surface:1").unwrap().apply_snapshot(20, 3, &history);
        assert_eq!(s.terms["surface:1"].display_offset(), 3);
        // Further scrolling is local, until back at the bottom.
        s.scroll_streamed("surface:1", -3);
        assert_eq!(s.terms["surface:1"].display_offset(), 0);
    }


    /// A streamed terminal hidden for a while gives up its view; shown again,
    /// it subscribes afresh and the snapshot brings the screen back.
    #[test]
    fn long_hidden_streamed_views_are_dropped() {
        let mut s = AppState::new();
        s.ensure_term("a");
        s.sync_streamed_terms(&HashSet::from(["a".to_owned()]));
        assert!(s.terms["a"].subscribed);
        s.sync_streamed_terms(&HashSet::new());
        assert!(!s.terms["a"].subscribed, "hidden: unsubscribed");
        assert!(s.terms.contains_key("a"), "a recently hidden view is kept");
        s.terms.get_mut("a").unwrap().hidden_since = Some(Instant::now() - HIDDEN_VIEW_TTL - Duration::from_secs(1));
        s.sync_streamed_terms(&HashSet::new());
        assert!(!s.terms.contains_key("a"), "a long-hidden view is dropped");
        s.pending.clear();
        s.ensure_term("a");
        s.sync_streamed_terms(&HashSet::from(["a".to_owned()]));
        assert!(s.pending.iter().any(|m| matches!(m, ClientMsg::Subscribe { surface, .. } if surface == "a")));
    }

    /// Terminals that asked for focus events hear when they gain or lose the
    /// keyboard; the rest are never sent a report.
    #[test]
    fn focus_reports_go_to_terminals_that_asked() {
        let mut s = state_with(&["a", "b", "c"], "a");
        for id in ["a", "b", "c"] {
            let tv = s.ensure_term(id);
            tv.subscribed = true;
            if id != "c" {
                tv.feed(b"\x1b[?1004h");
            }
        }
        let reports = |s: &mut AppState| -> Vec<(String, String)> {
            std::mem::take(&mut s.pending)
                .into_iter()
                .filter_map(|m| match m {
                    ClientMsg::TermInput { surface, data } => Some((surface, data)),
                    _ => None,
                })
                .collect()
        };
        s.sync_focus_reports(true);
        assert_eq!(reports(&mut s), vec![("b".to_owned(), "\x1b[O".to_owned())]);
        s.sync_focus_reports(true);
        assert!(reports(&mut s).is_empty(), "a report is sent once");
        s.sync_focus_reports(false);
        assert_eq!(reports(&mut s), vec![("a".to_owned(), "\x1b[O".to_owned())]);
        s.sync_focus_reports(true);
        assert_eq!(reports(&mut s), vec![("a".to_owned(), "\x1b[I".to_owned())]);
    }

    fn term(id: &str) -> Surface {
        Surface {
            id: id.into(),
            title: id.into(),
            kind: SurfaceKind::Terminal { tmux_pane: None, cols: 80, rows: 24 },
        }
    }

    /// One workspace, one pane holding `ids`, with `active` selected.
    fn state_with(ids: &[&str], active: &str) -> AppState {
        let pane = Pane {
            id: "pane:1".into(),
            surfaces: ids.iter().map(|i| term(i)).collect(),
            active_surface: Some(active.into()),
        };
        let ws = Workspace {
            id: "workspace:1".into(),
            title: "w".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: "pane:1".into() },
            panes: vec![pane],
            focused_pane: Some("pane:1".into()),
            ports: vec![],
            pr: None,
        };
        let tree = Tree {
            host: "h".into(),
            folders: vec![],
            workspaces: vec![ws],
            selected_workspace: Some("workspace:1".into()),
        };
        let mut st = AppState::new();
        st.apply_tree(tree);
        st
    }

    fn closed_surfaces(st: &AppState) -> Vec<String> {
        st.pending
            .iter()
            .filter_map(|m| match m {
                ClientMsg::Rpc { request } if request.method == "surface.close" => {
                    request.params["surface"].as_str().map(str::to_owned)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn closing_the_active_middle_chip_focuses_the_right_neighbour() {
        let mut st = state_with(&["s1", "s2", "s3"], "s2");
        st.close_surface("pane:1", "s2");
        assert_eq!(closed_surfaces(&st), vec!["s2"]);
        assert_eq!(st.pane_active.get("pane:1").map(String::as_str), Some("s3"));
    }

    #[test]
    fn closing_the_active_last_chip_focuses_the_left_neighbour() {
        let mut st = state_with(&["s1", "s2", "s3"], "s3");
        st.close_surface("pane:1", "s3");
        assert_eq!(st.pane_active.get("pane:1").map(String::as_str), Some("s2"));
    }

    #[test]
    fn closing_a_background_chip_leaves_the_selection_alone() {
        let mut st = state_with(&["s1", "s2", "s3"], "s2");
        st.close_surface("pane:1", "s3");
        assert_eq!(closed_surfaces(&st), vec!["s3"]);
        assert_eq!(st.pane_active.get("pane:1"), None);
        assert_eq!(st.active_surface(&st.pane("pane:1").unwrap().clone()).as_deref(), Some("s2"));
    }

    #[test]
    fn closing_the_only_chip_clears_the_override() {
        let mut st = state_with(&["s1"], "s1");
        st.close_surface("pane:1", "s1");
        assert_eq!(st.pane_active.get("pane:1"), None);
    }

    #[test]
    fn close_active_chip_targets_the_focused_pane() {
        let mut st = state_with(&["s1", "s2"], "s2");
        st.close_active_chip();
        assert_eq!(closed_surfaces(&st), vec!["s2"]);
        assert_eq!(st.pane_active.get("pane:1").map(String::as_str), Some("s1"));
    }

    #[test]
    fn resize_debounce_collapses_a_storm() {
        use std::time::{Duration, Instant};
        let mut b = BrowserView::new("u".into());
        b.subscribed = true;
        let d = Duration::from_millis(150);
        let t0 = Instant::now();

        // The first size starts the stream immediately, without a debounce wait.
        assert!(b.resize_decision((800, 600, 1.0), t0, d).0);

        // A window-edge drag sends a stream of sizes; none of them go out.
        for (i, w) in [(810, 600, 1.0), (830, 600, 1.0), (900, 600, 1.0)].iter().enumerate() {
            let (send, schedule) =
                b.resize_decision(*w, t0 + Duration::from_millis(10 * (i as u64 + 1)), d);
            assert!(!send, "an in-flight resize must not be sent");
            assert!(schedule.is_some(), "the debounce must keep a timer alive");
        }

        // Once the size has held for the window, the settled size goes out once.
        assert!(b.resize_decision((900, 600, 1.0), t0 + Duration::from_millis(200), d).0);
        assert!(!b.resize_decision((900, 600, 1.0), t0 + Duration::from_millis(400), d).0);
    }

    #[test]
    fn local_panes_are_never_streamed_from_the_daemon() {
        let mut st = state_with(&["s1"], "s1");
        // Give the surface the tmux window it would have in a real tree, and hold
        // the attach off so the test never starts a tmux client: the real attach
        // is covered by the PTY benchmark, which uses a scratch socket.
        if let SurfaceKind::Terminal { tmux_pane, .. } =
            &mut st.tree.workspaces[0].panes[0].surfaces[0].kind
        {
            *tmux_pane = Some("%7".into());
        }
        st.set_local_mode(true);
        st.ensure_term("s1");
        st.local_retry.insert("s1".into(), Instant::now() + Duration::from_secs(3600));
        st.visible.insert("s1".into());
        st.desired_sizes.insert("s1".into(), (100, 30));

        st.sync_subscriptions(true);
        assert!(
            st.pending.is_empty(),
            "a pane on this machine must not be subscribed, unsubscribed or resized \
             through the daemon: {:?}",
            st.pending
        );
        assert!(!st.terms["s1"].subscribed, "nothing is streamed for a local pane");
        assert!(st.terms["s1"].wants_visible, "the pane is still tracked as visible");
        assert!(st.local_terminal("s1"), "a terminal with a tmux window is a local pane");

        // Browsers have no local path: they keep streaming from the daemon.
        st.browsers.insert("b1".into(), BrowserView::new("https://example.com/".into()));
        st.visible.insert("b1".into());
        st.sync_subscriptions(true);
        assert!(
            st.pending
                .iter()
                .any(|m| matches!(m, ClientMsg::Subscribe { surface, .. } if surface == "b1")),
            "a browser pane still subscribes on the local host: {:?}",
            st.pending
        );
    }

    #[test]
    fn a_new_chip_becomes_active() {
        let mut st = state_with(&["s1", "s2"], "s2");
        st.select_tab("pane:1", "s1");
        assert_eq!(st.pane_active.get("pane:1").map(String::as_str), Some("s1"));
        st.new_terminal_chip("pane:1");
        assert_eq!(
            st.pane_active.get("pane:1"),
            None,
            "opening a chip must let the daemon's new active surface win"
        );
    }

    #[test]
    fn rename_round_trip_state() {
        let mut st = state_with(&["s1"], "s1");
        st.begin_rename("s1", "old");
        assert_eq!(st.renaming.as_ref().map(|r| r.text.as_str()), Some("old"));
        st.close_surface("pane:1", "s1");
        assert!(st.renaming.is_none(), "closing the chip must end the edit");
    }

    #[test]
    fn a_folder_create_reply_opens_its_rename_editor() {
        let mut st = AppState::new();
        st.new_folder(Some("folder:1".into()));
        let (id, params) = match st.pending.last().unwrap() {
            ClientMsg::Rpc { request } => {
                assert_eq!(request.method, "folder.new");
                (request.id, request.params.clone())
            }
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(params["parent"], "folder:1", "the new folder nests where asked");
        assert!(st.folder_rename.is_none(), "no editor until the ref is known");

        st.handle_server_msg(ServerMsg::RpcResult {
            response: lmux_proto::RpcResponse {
                id,
                result: Some(serde_json::json!({ "folder": "folder:9" })),
                error: None,
            },
        });
        let ren = st.folder_rename.as_ref().expect("editor opens on the new folder");
        assert_eq!((ren.folder.as_str(), ren.text.as_str()), ("folder:9", "New Folder"));
    }

    #[test]
    fn a_folder_that_disappears_loses_its_rename_editor() {
        let mut st = AppState::new();
        st.begin_folder_rename("folder:9", "New Folder");
        assert!(st.folder_rename.is_some());
        st.apply_tree(Tree::default());
        assert!(st.folder_rename.is_none(), "the editor cannot outlive its row");
    }

    #[test]
    fn the_click_before_a_double_click_is_taken_back() {
        let mut st = AppState::new();
        st.toggle_folder("folder:1", 10.0);
        assert!(st.collapsed.contains("folder:1"), "a click collapses the folder");

        assert!(st.take_back_folder_toggle("folder:1", 0.3, 10.1), "inside the window");
        assert!(!st.collapsed.contains("folder:1"), "the collapse is undone");
        assert!(
            !st.take_back_folder_toggle("folder:1", 0.3, 10.2),
            "a flip can only be taken back once"
        );
    }

    #[test]
    fn a_folder_flip_older_than_the_window_is_left_alone() {
        let mut st = AppState::new();
        st.toggle_folder("folder:1", 10.0);
        assert!(!st.take_back_folder_toggle("folder:1", 0.3, 10.4), "too late to be this gesture");
        assert!(st.collapsed.contains("folder:1"), "the click stands");
    }

    #[test]
    fn a_folder_trail_names_its_whole_path() {
        let mut st = AppState::new();
        st.apply_tree(Tree {
            host: "h".into(),
            folders: vec![
                lmux_proto::Folder {
                    id: "folder:1".into(),
                    name: "outer".into(),
                    parent: None,
                    collapsed: false,
                },
                lmux_proto::Folder {
                    id: "folder:2".into(),
                    name: "inner".into(),
                    parent: Some("folder:1".into()),
                    collapsed: false,
                },
            ],
            workspaces: vec![],
            selected_workspace: None,
        });
        assert_eq!(st.folder_path("folder:2"), "outer / inner");
        assert_eq!(st.folder_path("folder:1"), "outer");
        assert_eq!(st.folder_path("folder:404"), "");
        assert_eq!(st.folder_children(Some("folder:1")).len(), 1);
        assert!(st.folder_has_children("folder:1"));
    }

    #[test]
    fn split_chip_is_a_horizontal_terminal_split_of_that_pane() {
        let mut st = state_with(&["s1"], "s1");
        st.rpc(
            m::SPLIT_NEW,
            serde_json::json!({ "side": "right", "type": "terminal", "pane": "pane:1" }),
        );
        match st.pending.last().unwrap() {
            ClientMsg::Rpc { request } => {
                assert_eq!(request.method, "pane.split");
                assert_eq!(request.params["pane"], "pane:1");
            }
            other => panic!("unexpected {other:?}"),
        }
        let _ = SplitDirection::Horizontal;
        let _ = Progress { value: 0.0, label: None };
    }

    /// Folders `a` and `b` at the top level, `c` inside `b`; sessions `w1` in
    /// `a`, `w4` in `b`, and `w2`/`w3` at the top level, in that list order.
    fn sidebar_tree() -> Tree {
        use lmux_proto::Folder;
        let ws = |id: &str, folder: Option<&str>| Workspace {
            id: id.into(),
            title: id.into(),
            folder: folder.map(str::to_owned),
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: "pane:1".into() },
            panes: vec![],
            focused_pane: None,
            ports: vec![],
            pr: None,
        };
        Tree {
            host: "h".into(),
            folders: vec![
                Folder { id: "folder:a".into(), name: "a".into(), parent: None, collapsed: false },
                Folder { id: "folder:b".into(), name: "b".into(), parent: None, collapsed: false },
                Folder {
                    id: "folder:c".into(),
                    name: "c".into(),
                    parent: Some("folder:b".into()),
                    collapsed: false,
                },
            ],
            workspaces: vec![
                ws("w1", Some("folder:a")),
                ws("w2", None),
                ws("w3", None),
                ws("w4", Some("folder:b")),
            ],
            selected_workspace: None,
        }
    }

    #[test]
    fn a_workspace_slot_maps_into_the_list_the_daemon_inserts_into() {
        let t = sidebar_tree();
        // Into folder:a, in front of its only session.
        assert_eq!(workspace_move_index(&t, "w3", Some("folder:a"), Some(0)), Some(0));
        // Into folder:a behind that session: nothing of the folder follows, so
        // the index is left out and the daemon appends the session.
        assert_eq!(workspace_move_index(&t, "w1", Some("folder:a"), Some(1)), None);
        // To the top level, in front of w2 (the first top-level session).
        assert_eq!(workspace_move_index(&t, "w4", None, Some(0)), Some(1));
        // To the top level, at the end.
        assert_eq!(workspace_move_index(&t, "w1", None, None), None);
    }

    #[test]
    fn a_folder_slot_maps_into_the_list_the_daemon_inserts_into() {
        let t = sidebar_tree();
        // folder:c climbs out of folder:b, in front of folder:a.
        assert_eq!(folder_move_index(&t, "folder:c", None, Some(0)), Some(0));
        // The same move, landing between folder:a and folder:b.
        assert_eq!(folder_move_index(&t, "folder:c", None, Some(1)), Some(1));
        // Into folder:b, which holds no other folder: the index is left out.
        assert_eq!(folder_move_index(&t, "folder:c", Some("folder:b"), Some(0)), None);
    }

    #[test]
    fn a_drop_beside_itself_is_not_a_move() {
        assert_eq!(insertion_slot(2, None), Some(2), "another container always inserts");
        assert_eq!(insertion_slot(2, Some(2)), None, "dropping on itself is no move");
        assert_eq!(insertion_slot(3, Some(2)), None, "nor is dropping just behind itself");
        assert_eq!(insertion_slot(4, Some(2)), Some(3), "past itself the slot shifts down");
        assert_eq!(insertion_slot(0, Some(2)), Some(0), "moving up is unaffected");
    }

    #[test]
    fn a_folder_holds_itself_and_its_subtree() {
        let t = sidebar_tree();
        assert!(folder_holds(&t, "folder:b", Some("folder:c")));
        assert!(folder_holds(&t, "folder:b", Some("folder:b")));
        assert!(!folder_holds(&t, "folder:c", Some("folder:b")));
        assert!(!folder_holds(&t, "folder:c", None), "the top level is nobody's subtree");
        assert!(!folder_holds(&t, "folder:a", Some("folder:b")));
    }
}

/// The `index` that drops a dragged workspace into `folder` at `slot`, where the
/// slot counts that folder's workspaces. `workspace.move` removes the workspace
/// before it inserts, so the slot and the index are both counted in the list
/// without it. `None` leaves the index out, and the daemon appends.
pub fn workspace_move_index(
    tree: &Tree,
    dragged: &str,
    folder: Option<&str>,
    slot: Option<usize>,
) -> Option<usize> {
    let slot = slot?;
    let rest: Vec<&lmux_proto::Workspace> =
        tree.workspaces.iter().filter(|w| w.id != dragged).collect();
    slot_to_index(&rest, |w| w.folder.as_deref() == folder, slot)
}

/// The `index` that drops a dragged folder under `parent` at `slot`, counted the
/// same way as [`workspace_move_index`].
pub fn folder_move_index(
    tree: &Tree,
    dragged: &str,
    parent: Option<&str>,
    slot: Option<usize>,
) -> Option<usize> {
    let slot = slot?;
    let rest: Vec<&lmux_proto::Folder> =
        tree.folders.iter().filter(|f| f.id != dragged).collect();
    slot_to_index(&rest, |f| f.parent.as_deref() == parent, slot)
}

/// Map a slot among a container's rows to the position in the list the daemon
/// inserts into. `rest` is that list without the moved item, so a same-container
/// reorder does not count it; a slot past the end lands after the last member.
fn slot_to_index<T>(rest: &[&T], is_member: impl Fn(&T) -> bool, slot: usize) -> Option<usize> {
    let members: Vec<usize> = rest
        .iter()
        .enumerate()
        .filter(|(_, item)| is_member(item))
        .map(|(i, _)| i)
        .collect();
    match members.get(slot) {
        Some(at) => Some(*at),
        None => members.last().map(|last| last + 1),
    }
}

/// Where a workspace sits among the workspaces of `folder`, in tree order.
pub fn workspace_position(tree: &Tree, id: &str, folder: Option<&str>) -> Option<usize> {
    tree.workspaces
        .iter()
        .filter(|w| w.folder.as_deref() == folder)
        .position(|w| w.id == id)
}

/// Where a folder sits among the child folders of `parent`.
pub fn folder_position(tree: &Tree, id: &str, parent: Option<&str>) -> Option<usize> {
    tree.folders
        .iter()
        .filter(|f| f.parent.as_deref() == parent)
        .position(|f| f.id == id)
}

/// Whether `ancestor` is `candidate` itself or holds it somewhere below, so a
/// folder cannot be dropped into its own subtree. `folder.move` refuses that
/// move too; catching it here keeps the indicator from promising it.
pub fn folder_holds(tree: &Tree, ancestor: &str, candidate: Option<&str>) -> bool {
    let mut cursor = candidate.map(str::to_owned);
    // The folder list is finite, so a malformed cycle cannot loop forever.
    for _ in 0..=tree.folders.len() {
        let Some(current) = cursor else { return false };
        if current == ancestor {
            return true;
        }
        cursor = tree
            .folders
            .iter()
            .find(|f| f.id == current)
            .and_then(|f| f.parent.clone());
    }
    false
}

/// The slot a row dropped between two rows of one container takes, given that
/// container's rows in list order. `target` is the slot *before* which the row
/// lands (the row above it plus one when the drop is below that row), and `own`
/// is the dragged row's own position in the same list. `None` means the row
/// would land exactly where it already is, so there is nothing to send and the
/// indicator stays off. The daemon removes the row before inserting, so a slot
/// after the row's own position shifts down by one.
pub fn insertion_slot(target: usize, own: Option<usize>) -> Option<usize> {
    if let Some(own) = own {
        if target == own || target == own + 1 {
            return None;
        }
        if target > own {
            return Some(target - 1);
        }
    }
    Some(target)
}

/// A folder's trail inside any folder list, `webapp / api`. The trail ends
/// early if the list is inconsistent, so a bad tree cannot loop.
pub fn folder_path_in(folders: &[lmux_proto::Folder], id: &str) -> String {
    let mut parts = Vec::new();
    let mut cursor = Some(id.to_owned());
    for _ in 0..folders.len().max(1) {
        let Some(current) = cursor.take() else { break };
        let Some(folder) = folders.iter().find(|f| f.id == current) else {
            break;
        };
        parts.push(folder.name.clone());
        cursor = folder.parent.clone();
    }
    parts.reverse();
    parts.join(" / ")
}

/// Small helper so the UI can print an error code without importing serde's enum.
trait RpcErrorExt {
    fn code_str(&self) -> String;
}

impl RpcErrorExt for lmux_proto::RpcError {
    fn code_str(&self) -> String {
        format!("{:?}", self.code).to_lowercase()
    }
}
