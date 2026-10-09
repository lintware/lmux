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
    pub sent_size: Option<(u32, u32, f32)>,
    /// A size change waiting out the resize debounce (review #9).
    pub pending_size: Option<(u32, u32, f32)>,
    pub pending_since: Option<std::time::Instant>,
    pub url_draft: Option<String>,
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
    pub terms: HashMap<Ref, TermView>,
    pub browsers: HashMap<Ref, BrowserView>,
    /// PTYs owned by this process, one per local terminal surface. Present only
    /// while the connected host is this machine (see [`AppState::local_mode`]).
    pub local_ptys: HashMap<Ref, LocalPty>,
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
    /// The collapse flip the folder row's last click made, for the double click
    /// that follows it.
    pub folder_toggle: Option<FolderToggle>,
    /// `folder.new` requests whose result should open the rename editor.
    folder_rename_pending: Option<u64>,
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
            pending_create: None,
            terms: HashMap::new(),
            browsers: HashMap::new(),
            local_ptys: HashMap::new(),
            local_mode: false,
            local_wanted: HashSet::new(),
            local_retry: HashMap::new(),
            wake: None,
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
    fn fire_pending_create(&mut self) {
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

    /// The globe / `Cmd+Shift+B`: a new browser chip in a pane.
    pub fn new_browser_chip(&mut self, pane: &str) {
        self.clear_tab_override(pane);
        self.rpc(
            m::SURFACE_NEW,
            serde_json::json!({ "pane": pane, "type": "browser", "url": "https://example.com/" }),
        );
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
            ServerMsg::RpcResult { response } => {
                if let Some(err) = response.error {
                    self.banner = Some((format!("{}: {}", err.code_str(), err.message), true));
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

    pub fn apply_tree(&mut self, tree: Tree) {
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
                crate::decode::decoder().submit(surface.to_owned(), seq, jpeg.to_vec());
            }
            Err(e) => eprintln!("[frame] decode failed: {e:?}"),
        }
    }

    /// Collect frames finished by the decode worker and queue an ack for the
    /// one that replaced a surface's image. Called once per pump.
    pub fn poll_decoded(&mut self) {
        for frame in crate::decode::decoder().drain() {
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
                self.pending.push(ClientMsg::Subscribe { surface: id.clone() });
            } else if !visible_now && b.subscribed {
                b.subscribed = false;
                self.pending.push(ClientMsg::Unsubscribe { surface: id.clone() });
            }
        }
    }

    /// Terminals the daemon streams: subscribe while visible, unsubscribe while
    /// hidden, and let the daemon resize their tmux windows.
    fn sync_streamed_terms(&mut self, visible: &HashSet<Ref>) {
        let ids: Vec<Ref> = self.terms.keys().cloned().collect();
        for id in &ids {
            let visible_now = visible.contains(id);
            let Some(t) = self.terms.get_mut(id) else { continue };
            t.wants_visible = visible_now;
            if visible_now && !t.subscribed {
                t.subscribed = true;
                self.pending.push(ClientMsg::Subscribe { surface: id.clone() });
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

            if self.local_ptys.contains_key(id) {
                let size = self.desired_sizes.get(id).copied().unwrap_or((80, 24));
                if let Some(p) = self.local_ptys.get_mut(id) {
                    p.resize(size.0, size.1);
                }
                if let Some(t) = self.terms.get_mut(id) {
                    t.resize(size.0, size.1);
                }
                continue;
            }

            if !self.local_wanted.contains(id) {
                continue;
            }
            if self.local_retry.get(id).is_some_and(|t| *t > now) {
                continue;
            }
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
                self.pending.push(ClientMsg::Unsubscribe { surface: id.clone() });
            }
        }
    }

    /// Called when the connection drops: forget what we thought was subscribed.
    pub fn reset_subscriptions(&mut self) {
        for t in self.terms.values_mut() {
            t.subscribed = false;
        }
        for b in self.browsers.values_mut() {
            b.subscribed = false;
        }
    }

    pub fn ensure_term(&mut self, surface: &str) -> &mut TermView {
        self.terms.entry(surface.to_owned()).or_insert_with(|| TermView::new(80, 24))
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
                .any(|m| matches!(m, ClientMsg::Subscribe { surface } if surface == "b1")),
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
