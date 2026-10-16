//! In-memory session tree: folders, workspaces, panes, surfaces, plus the pure
//! layout operations. Everything the daemon exposes lives here; tmux and browsers
//! are driven by callers that first allocate refs here.

use std::collections::HashMap;
use std::time::Instant;

use lmux_proto::*;
use serde::{Deserialize, Serialize};

/// Refs never repeat during a daemon's life. Persisted so restarts keep going up.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Counters {
    pub folder: u64,
    pub workspace: u64,
    pub pane: u64,
    pub surface: u64,
    pub notification: u64,
}

/// Which surfaces were affected by a mutation, so the caller can tear down tmux
/// windows / browser sessions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Removed {
    pub surfaces: Vec<Ref>,
    pub workspace_closed: Option<Ref>,
}

/// Errors from the drag-and-drop rearranging ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveError {
    SurfaceNotFound,
    PaneNotFound,
    DifferentWorkspace,
    /// The surface is the only one in its workspace, so moving it out into a
    /// new workspace would change nothing.
    OnlySurface,
}

/// Why a folder cannot be reparented. A cycle would tear the subtree loose from
/// the tree, so the daemon refuses it rather than dropping the folder's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveFolderError {
    NotFound,
    ParentNotFound,
    Cycle,
}

/// Why a typed folder name names no single folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderLookupError {
    NotFound,
    /// Several folders share the title; these are their refs.
    Ambiguous(Vec<Ref>),
}

/// Which surface a tmux window/pane backs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TermMeta {
    pub window: String,
    pub pane: String,
}

/// An agent session a terminal can resume after its tmux pane is lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Which agent, e.g. `claude`. Decides the resume command.
    pub agent: String,
    /// The agent's own session id (a UUID for Claude).
    pub id: String,
}

/// What a terminal needs to come back after its tmux pane is lost (tmux
/// server killed, machine rebooted). Kept beside the tree rather than in
/// `SurfaceKind`, so the wire format and the app are unaffected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermResume {
    /// Last known working directory (tmux `pane_current_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Agent to resume when the terminal is respawned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentSession>,
}

/// Agent session ids are typed into a shell, so only a conservative alphabet
/// is accepted (Claude's are UUIDs).
pub fn valid_agent_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub host: String,
    #[serde(default)]
    pub folders: Vec<Folder>,
    pub workspaces: Vec<Workspace>,
    pub selected_workspace: Option<Ref>,
    #[serde(default)]
    pub counters: Counters,
    #[serde(default)]
    pub notifications: Vec<Notification>,
    #[serde(default)]
    pub logs: Vec<LogEntry>,
    /// Per terminal surface: cwd and agent session, for respawn after tmux is
    /// lost. Pruned to live surfaces by `prune_resume`.
    #[serde(default)]
    pub resume: HashMap<Ref, TermResume>,
    /// Content-free delivery observations, bounded per live terminal. They
    /// expire with the daemon; restart never resurrects a receipt claim.
    #[serde(skip)]
    pub harnesses: HashMap<Ref, crate::harness::Harness>,
    // Runtime indexes over `workspaces` (not persisted; rebuilt on reconcile).
    // Keeping them here means one lock guards the whole session, rather than a
    // lock per index with an ordering hazard.
    #[serde(skip)]
    pub terms: HashMap<Ref, TermMeta>,
    /// tmux window id -> surface.
    #[serde(skip)]
    pub windows: HashMap<String, Ref>,
    /// tmux pane id (`%N`) -> surface, so output routing is an O(1) lookup.
    #[serde(skip)]
    pub pane_index: HashMap<String, Ref>,
    /// cwd -> (branch, detected_at); short TTL so tree sends stay cheap.
    #[serde(skip)]
    pub branches: HashMap<String, (Option<String>, Instant)>,
}

const MAX_LOGS: usize = 2000;

impl SessionState {
    pub fn new(host: impl Into<String>) -> Self {
        SessionState {
            host: host.into(),
            ..Default::default()
        }
    }

    // --- refs -------------------------------------------------------------

    pub fn next_folder(&mut self) -> Ref {
        self.counters.folder += 1;
        format!("folder:{}", self.counters.folder)
    }
    pub fn next_workspace(&mut self) -> Ref {
        self.counters.workspace += 1;
        format!("workspace:{}", self.counters.workspace)
    }
    pub fn next_pane(&mut self) -> Ref {
        self.counters.pane += 1;
        format!("pane:{}", self.counters.pane)
    }
    pub fn next_surface(&mut self) -> Ref {
        self.counters.surface += 1;
        format!("surface:{}", self.counters.surface)
    }

    /// Bump a counter to at least `n` so refs from a previous run never repeat.
    pub fn observe_ref(&mut self, r: &str) {
        let n = r.rsplit(':').next().and_then(|s| s.parse::<u64>().ok());
        let (slot, n) = match r.split(':').next() {
            Some("folder") => (&mut self.counters.folder, n),
            Some("workspace") => (&mut self.counters.workspace, n),
            Some("pane") => (&mut self.counters.pane, n),
            Some("surface") => (&mut self.counters.surface, n),
            _ => return,
        };
        if let Some(n) = n {
            if n > *slot {
                *slot = n;
            }
        }
    }

    // --- lookup -----------------------------------------------------------

    pub fn folder(&self, id: &str) -> Option<&Folder> {
        self.folders.iter().find(|f| f.id == id)
    }

    pub fn workspace(&self, id: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn workspace_mut(&mut self, id: &str) -> Option<&mut Workspace> {
        self.workspaces.iter_mut().find(|w| w.id == id)
    }

    pub fn workspace_index(&self, id: &str) -> Option<usize> {
        self.workspaces.iter().position(|w| w.id == id)
    }

    /// (workspace index, pane index) for the pane holding `surface`.
    pub fn locate_surface(&self, surface: &str) -> Option<(usize, usize)> {
        for (wi, w) in self.workspaces.iter().enumerate() {
            for (pi, p) in w.panes.iter().enumerate() {
                if p.surfaces.iter().any(|s| s.id == surface) {
                    return Some((wi, pi));
                }
            }
        }
        None
    }

    pub fn locate_pane(&self, pane: &str) -> Option<(usize, usize)> {
        for (wi, w) in self.workspaces.iter().enumerate() {
            if let Some(pi) = w.panes.iter().position(|p| p.id == pane) {
                return Some((wi, pi));
            }
        }
        None
    }

    /// Workspace that owns `surface`, cloning the ref.
    pub fn workspace_of_surface(&self, surface: &str) -> Option<Ref> {
        self.locate_surface(surface)
            .map(|(wi, _)| self.workspaces[wi].id.clone())
    }

    /// Pane that owns `surface`.
    pub fn pane_of_surface(&self, surface: &str) -> Option<Ref> {
        self.locate_surface(surface)
            .map(|(wi, pi)| self.workspaces[wi].panes[pi].id.clone())
    }

    pub fn surface(&self, surface: &str) -> Option<&Surface> {
        let (wi, pi) = self.locate_surface(surface)?;
        self.workspaces[wi].panes[pi]
            .surfaces
            .iter()
            .find(|s| s.id == surface)
    }

    pub fn surface_cloned(&self, surface: &str) -> Option<Surface> {
        self.surface(surface).cloned()
    }

    pub fn surface_mut(&mut self, surface: &str) -> Option<&mut Surface> {
        let (wi, pi) = self.locate_surface(surface)?;
        self.workspaces[wi].panes[pi]
            .surfaces
            .iter_mut()
            .find(|s| s.id == surface)
    }

    pub fn all_surface_refs(&self) -> Vec<Ref> {
        self.workspaces
            .iter()
            .flat_map(|w| w.panes.iter())
            .flat_map(|p| p.surfaces.iter().map(|s| s.id.clone()))
            .collect()
    }

    // --- resume ----------------------------------------------------------

    /// Drop resume entries for surfaces that no longer exist.
    pub fn prune_resume(&mut self) {
        let live: std::collections::HashSet<Ref> = self.all_surface_refs().into_iter().collect();
        self.resume.retain(|k, _| live.contains(k));
        self.harnesses.retain(|k, _| live.contains(k));
    }

    pub fn resume_mut(&mut self, surface: &str) -> &mut TermResume {
        self.resume.entry(surface.to_string()).or_default()
    }

    // --- tmux index ------------------------------------------------------

    pub fn set_term(&mut self, surface: &str, window: &str, pane: &str) {
        let meta = TermMeta {
            window: window.to_string(),
            pane: pane.to_string(),
        };
        self.windows.insert(window.to_string(), surface.to_string());
        self.pane_index.insert(pane.to_string(), surface.to_string());
        self.terms.insert(surface.to_string(), meta);
    }

    pub fn term(&self, surface: &str) -> Option<&TermMeta> {
        self.terms.get(surface)
    }

    pub fn term_cloned(&self, surface: &str) -> Option<TermMeta> {
        self.terms.get(surface).cloned()
    }

    /// Which surface owns a tmux pane id (`%N`) — O(1).
    pub fn surface_of_pane(&self, pane: &str) -> Option<Ref> {
        self.pane_index.get(pane).cloned()
    }

    pub fn surface_of_window(&self, window: &str) -> Option<Ref> {
        self.windows.get(window).cloned()
    }

    pub fn remove_term(&mut self, surface: &str) -> Option<TermMeta> {
        let meta = self.terms.remove(surface);
        if let Some(m) = &meta {
            self.windows.remove(&m.window);
            self.pane_index.remove(&m.pane);
        }
        meta
    }

    pub fn remove_window(&mut self, window: &str) -> Option<Ref> {
        let surface = self.windows.get(window).cloned()?;
        self.remove_term(&surface);
        Some(surface)
    }

    /// First surface of a workspace's focused pane (fallback: first surface anywhere).
    pub fn focused_surface(&self, workspace: &str) -> Option<Ref> {
        let w = self.workspace(workspace)?;
        if let Some(fp) = &w.focused_pane {
            if let Some(p) = w.panes.iter().find(|p| &p.id == fp) {
                if let Some(s) = p.active_surface.clone().or_else(|| p.surfaces.first().map(|s| s.id.clone())) {
                    return Some(s);
                }
            }
        }
        w.panes
            .iter()
            .find_map(|p| p.active_surface.clone().or_else(|| p.surfaces.first().map(|s| s.id.clone())))
    }

    // --- folders ----------------------------------------------------------

    pub fn add_folder(&mut self, name: impl Into<String>, parent: Option<Ref>) -> Folder {
        let id = self.next_folder();
        let f = Folder {
            id,
            name: name.into(),
            parent,
            collapsed: false,
            cwd: None,
            host: (!self.host.is_empty()).then(|| self.host.clone()),
        };
        self.folders.push(f.clone());
        f
    }

    /// The directory a new workspace in `id` starts in: the folder's own cwd,
    /// else the nearest ancestor's. `None` keeps the daemon's default.
    pub fn folder_cwd(&self, id: &str) -> Option<String> {
        let mut cursor = Some(id.to_string());
        // Bounded like `is_descendant_of`, in case a cycle ever slips in.
        for _ in 0..=self.folders.len() {
            let f = self.folder(cursor.as_deref()?)?;
            if let Some(cwd) = f.cwd.as_ref().filter(|c| !c.is_empty()) {
                return Some(cwd.clone());
            }
            cursor = f.parent.clone();
        }
        None
    }

    /// How many workspaces and child folders sit directly in `id`: what a
    /// close would move up a level.
    pub fn folder_contents(&self, id: &str) -> (usize, usize) {
        let workspaces = self.workspaces.iter().filter(|w| w.folder.as_deref() == Some(id)).count();
        (workspaces, self.folder_children(Some(id)).len())
    }

    /// A folder named the way people type it: a ref (`folder:3`), its number
    /// (`3`), or its title when exactly one folder has it.
    pub fn resolve_folder(&self, name: &str) -> Result<Ref, FolderLookupError> {
        if self.folder(name).is_some() {
            return Ok(name.to_string());
        }
        if !name.is_empty() && name.bytes().all(|c| c.is_ascii_digit()) {
            let numbered = format!("folder:{name}");
            if self.folder(&numbered).is_some() {
                return Ok(numbered);
            }
        }
        let named: Vec<&Folder> = self.folders.iter().filter(|f| f.name == name).collect();
        match named.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(FolderLookupError::NotFound),
            many => Err(FolderLookupError::Ambiguous(many.iter().map(|f| f.id.clone()).collect())),
        }
    }

    /// Every folder inside `parent`, in list order (`None` = top level).
    pub fn folder_children(&self, parent: Option<&str>) -> Vec<&Folder> {
        self.folders
            .iter()
            .filter(|f| f.parent.as_deref() == parent)
            .collect()
    }

    /// Close `id` and lift its contents one level: its workspaces and its child
    /// folders move to the closed folder's parent. Nothing is discarded — a
    /// nested session stays reachable after its folder goes away.
    pub fn close_folder(&mut self, id: &str) -> bool {
        let Some(parent) = self.folder(id).map(|f| f.parent.clone()) else {
            return false;
        };
        self.folders.retain(|f| f.id != id);
        for w in &mut self.workspaces {
            if w.folder.as_deref() == Some(id) {
                w.folder = parent.clone();
            }
        }
        for f in &mut self.folders {
            if f.parent.as_deref() == Some(id) {
                f.parent = parent.clone();
            }
        }
        true
    }

    /// Reparent `id` under `parent` and put it at `index` among that parent's
    /// child folders (`None` appends). Reordering and nesting are the same
    /// operation. A folder cannot be moved into itself or into one of its own
    /// descendants: that would detach the subtree from the tree.
    pub fn move_folder(
        &mut self,
        id: &str,
        parent: Option<Ref>,
        index: Option<usize>,
    ) -> Result<(), MoveFolderError> {
        if self.folder(id).is_none() {
            return Err(MoveFolderError::NotFound);
        }
        if let Some(p) = &parent {
            if self.folder(p).is_none() {
                return Err(MoveFolderError::ParentNotFound);
            }
            if p == id || self.is_descendant_of(p, id) {
                return Err(MoveFolderError::Cycle);
            }
        }

        let Some(at) = self.folders.iter().position(|f| f.id == id) else {
            return Err(MoveFolderError::NotFound);
        };
        // Slots count the destination parent's children in the list as it looks
        // *after* the removal, so a same-parent reorder is not off by one.
        let rest: Vec<usize> = (0..self.folders.len()).filter(|i| *i != at).collect();
        let children: Vec<usize> = rest
            .iter()
            .enumerate()
            .filter(|(_, i)| self.folders[**i].parent == parent)
            .map(|(pos, _)| pos)
            .collect();
        let slot = match index {
            Some(i) => children.get(i).copied().unwrap_or(rest.len()),
            None => rest.len(),
        };

        let mut folder = self.folders.remove(at);
        folder.parent = parent;
        let target = slot.min(self.folders.len());
        self.folders.insert(target, folder);
        Ok(())
    }

    /// Whether `id` sits anywhere under `ancestor` (a folder is not its own descendant).
    fn is_descendant_of(&self, id: &str, ancestor: &str) -> bool {
        let mut cursor = self.folder(id).and_then(|f| f.parent.clone());
        // The folder list is finite, so the walk is bounded even if it has a cycle.
        for _ in 0..=self.folders.len() {
            let Some(current) = cursor else { return false };
            if current == ancestor {
                return true;
            }
            cursor = self.folder(&current).and_then(|f| f.parent.clone());
        }
        false
    }

    // --- workspaces -------------------------------------------------------

    pub fn add_workspace(&mut self, mut ws: Workspace) -> Ref {
        self.observe_ref(&ws.id);
        for p in &ws.panes {
            self.observe_ref(&p.id);
            for s in &p.surfaces {
                self.observe_ref(&s.id);
            }
        }
        let id = ws.id.clone();
        if ws.focused_pane.is_none() {
            ws.focused_pane = ws.panes.first().map(|p| p.id.clone());
        }
        self.workspaces.push(ws);
        id
    }

    pub fn remove_workspace(&mut self, id: &str) -> Option<Workspace> {
        let idx = self.workspace_index(id)?;
        let ws = self.workspaces.remove(idx);
        if self.selected_workspace.as_deref() == Some(id) {
            self.selected_workspace = self.workspaces.first().map(|w| w.id.clone());
        }
        Some(ws)
    }

    pub fn move_workspace(&mut self, id: &str, folder: Option<Ref>, index: Option<usize>) -> bool {
        let Some(idx) = self.workspace_index(id) else {
            return false;
        };
        let mut ws = self.workspaces.remove(idx);
        ws.folder = folder;
        let at = index.unwrap_or(self.workspaces.len()).min(self.workspaces.len());
        self.workspaces.insert(at, ws);
        true
    }

    // --- panes / surfaces -------------------------------------------------

    pub fn set_pane_surface_size(&mut self, surface: &str, cols: u16, rows: u16) {
        if let Some(s) = self.surface_mut(surface) {
            if let SurfaceKind::Terminal {
                cols: c, rows: r, ..
            } = &mut s.kind
            {
                *c = cols;
                *r = rows;
            }
        }
    }

    pub fn set_surface_tmux(&mut self, surface: &str, pane: &str) {
        if let Some(s) = self.surface_mut(surface) {
            if let SurfaceKind::Terminal { tmux_pane, .. } = &mut s.kind {
                *tmux_pane = Some(pane.to_string());
            }
        }
    }

    /// Replace the leaf `target` with a split holding `target` and `new_pane`.
    pub fn split_pane(
        &mut self,
        workspace: &str,
        target_pane: &str,
        side: SplitSide,
        new_pane: &str,
    ) -> bool {
        let Some(w) = self.workspace_mut(workspace) else {
            return false;
        };
        split_layout(&mut w.layout, target_pane, new_pane, side)
    }

    /// Set the ratio of the split whose child is `pane`.
    pub fn resize_pane(&mut self, workspace: &str, pane: &str, ratio: f32) -> bool {
        let ratio = ratio.clamp(0.05, 0.95);
        let Some(w) = self.workspace_mut(workspace) else {
            return false;
        };
        set_split_ratio(&mut w.layout, pane, ratio)
    }

    pub fn add_surface(&mut self, pane_ref: &str, surface: Surface) -> bool {
        self.observe_ref(&surface.id);
        let Some((wi, pi)) = self.locate_pane(pane_ref) else {
            return false;
        };
        let pane = &mut self.workspaces[wi].panes[pi];
        pane.active_surface = Some(surface.id.clone());
        pane.surfaces.push(surface);
        self.workspaces[wi].focused_pane = Some(pane_ref.to_string());
        true
    }

    pub fn set_active_surface(&mut self, surface: &str) -> bool {
        let Some((wi, pi)) = self.locate_surface(surface) else {
            return false;
        };
        self.workspaces[wi].panes[pi].active_surface = Some(surface.to_string());
        self.workspaces[wi].focused_pane = Some(self.workspaces[wi].panes[pi].id.clone());
        true
    }

    /// Remove a surface. Drops the pane when its last surface goes, collapses the
    /// split, and closes the workspace when its last pane goes.
    pub fn remove_surface(&mut self, surface: &str) -> Removed {
        let mut removed = Removed::default();
        let Some((wi, pi)) = self.locate_surface(surface) else {
            return removed;
        };
        removed.surfaces.push(surface.to_string());
        let pane_ref = self.workspaces[wi].panes[pi].id.clone();

        // Drop the surface and fix the active tab.
        {
            let pane = &mut self.workspaces[wi].panes[pi];
            pane.surfaces.retain(|s| s.id != surface);
            if pane.active_surface.as_deref() == Some(surface) {
                pane.active_surface = pane.surfaces.first().map(|s| s.id.clone());
            }
        }

        if !self.workspaces[wi].panes[pi].surfaces.is_empty() {
            return removed;
        }

        // Pane is empty: remove it from the layout.
        let ws_ref = self.workspaces[wi].id.clone();
        let layout = std::mem::replace(
            &mut self.workspaces[wi].layout,
            Layout::Pane {
                pane: String::new(),
            },
        );
        match remove_pane_layout(layout, &pane_ref) {
            Some(new_layout) => {
                let ws = &mut self.workspaces[wi];
                ws.layout = new_layout;
                ws.panes.retain(|p| p.id != pane_ref);
                if ws.focused_pane.as_deref() == Some(&pane_ref) {
                    ws.focused_pane = ws.panes.first().map(|p| p.id.clone());
                }
            }
            None => {
                // No panes left: close the workspace.
                self.remove_workspace(&ws_ref);
                removed.workspace_closed = Some(ws_ref);
            }
        }
        removed
    }

    /// Remove a whole pane (kills all its surfaces).
    pub fn remove_pane(&mut self, pane_ref: &str) -> Removed {
        let mut removed = Removed::default();
        let Some((wi, pi)) = self.locate_pane(pane_ref) else {
            return removed;
        };
        let surfaces: Vec<Ref> = self.workspaces[wi].panes[pi]
            .surfaces
            .iter()
            .map(|s| s.id.clone())
            .collect();
        let ws_ref = self.workspaces[wi].id.clone();
        let layout = std::mem::replace(
            &mut self.workspaces[wi].layout,
            Layout::Pane {
                pane: String::new(),
            },
        );
        match remove_pane_layout(layout, pane_ref) {
            Some(new_layout) => {
                let ws = &mut self.workspaces[wi];
                ws.layout = new_layout;
                ws.panes.retain(|p| p.id != pane_ref);
                if ws.focused_pane.as_deref() == Some(pane_ref) {
                    ws.focused_pane = ws.panes.first().map(|p| p.id.clone());
                }
            }
            None => {
                self.remove_workspace(&ws_ref);
                removed.workspace_closed = Some(ws_ref);
            }
        }
        removed.surfaces = surfaces;
        removed
    }

    // --- drag-and-drop rearranging ---------------------------------------

    /// Move a surface (tab) into another pane, or reorder it within its own pane.
    /// `index` defaults to the end and is clamped. The surface keeps its backing
    /// (tmux window / browser target); nothing restarts. A source pane left empty
    /// is removed and its split collapsed.
    pub fn move_surface(
        &mut self,
        surface: &str,
        target_pane: &str,
        index: Option<usize>,
    ) -> Result<Ref, MoveError> {
        let (s_wi, s_pi) = self
            .locate_surface(surface)
            .ok_or(MoveError::SurfaceNotFound)?;
        let (t_wi, _) = self.locate_pane(target_pane).ok_or(MoveError::PaneNotFound)?;
        if s_wi != t_wi {
            return Err(MoveError::DifferentWorkspace);
        }
        let ws = &mut self.workspaces[s_wi];
        let source_pane_id = ws.panes[s_pi].id.clone();

        if source_pane_id == target_pane {
            // Reorder in place. Removing first shifts the indices, so clamp against
            // the length after removal.
            let pane = &mut ws.panes[s_pi];
            let pos = pane
                .surfaces
                .iter()
                .position(|s| s.id == surface)
                .ok_or(MoveError::SurfaceNotFound)?;
            let item = pane.surfaces.remove(pos);
            let len = pane.surfaces.len();
            let at = index.map(|i| i.min(len)).unwrap_or(len);
            pane.surfaces.insert(at, item);
            pane.active_surface = Some(surface.to_string());
            ws.focused_pane = Some(target_pane.to_string());
            return Ok(target_pane.to_string());
        }

        // Move to another pane in the same workspace.
        let pos = ws.panes[s_pi]
            .surfaces
            .iter()
            .position(|s| s.id == surface)
            .ok_or(MoveError::SurfaceNotFound)?;
        let item = ws.panes[s_pi].surfaces.remove(pos);
        {
            let src = &mut ws.panes[s_pi];
            if src.active_surface.as_deref() == Some(surface) {
                src.active_surface = src.surfaces.first().map(|s| s.id.clone());
            }
        }
        let t_pi = ws
            .panes
            .iter()
            .position(|p| p.id == target_pane)
            .ok_or(MoveError::PaneNotFound)?;
        {
            let tp = &mut ws.panes[t_pi];
            let len = tp.surfaces.len();
            let at = index.map(|i| i.min(len)).unwrap_or(len);
            tp.surfaces.insert(at, item);
            tp.active_surface = Some(surface.to_string());
        }
        ws.focused_pane = Some(target_pane.to_string());

        if ws.panes[s_pi].surfaces.is_empty() {
            let layout = std::mem::replace(
                &mut ws.layout,
                Layout::Pane {
                    pane: String::new(),
                },
            );
            match remove_pane_layout(layout, &source_pane_id) {
                Some(new_layout) => {
                    ws.layout = new_layout;
                    ws.panes.retain(|p| p.id != source_pane_id);
                }
                None => {
                    // The target pane exists, so the layout is never empty here.
                    ws.layout = Layout::Pane {
                        pane: target_pane.to_string(),
                    };
                }
            }
        }
        Ok(target_pane.to_string())
    }

    /// Move an existing surface into a **new** pane split off its current pane, on
    /// `side` (left/up put the new pane first). A pane holding only this surface
    /// has nothing to divide, so it is a no-op returning the existing pane.
    pub fn split_surface(&mut self, surface: &str, side: SplitSide) -> Result<Ref, MoveError> {
        let (wi, pi) = self
            .locate_surface(surface)
            .ok_or(MoveError::SurfaceNotFound)?;
        let ws_ref = self.workspaces[wi].id.clone();

        if self.workspaces[wi].panes[pi].surfaces.len() < 2 {
            return Ok(self.workspaces[wi].panes[pi].id.clone());
        }
        let source_pane = self.workspaces[wi].panes[pi].id.clone();
        let new_pane = self.next_pane();

        let item = {
            let src = &mut self.workspaces[wi].panes[pi];
            let pos = src
                .surfaces
                .iter()
                .position(|s| s.id == surface)
                .ok_or(MoveError::SurfaceNotFound)?;
            let item = src.surfaces.remove(pos);
            if src.active_surface.as_deref() == Some(surface) {
                src.active_surface = src.surfaces.first().map(|s| s.id.clone());
            }
            item
        };
        self.workspaces[wi].panes.push(Pane {
            id: new_pane.clone(),
            surfaces: vec![item],
            active_surface: Some(surface.to_string()),
        });
        self.split_pane(&ws_ref, &source_pane, side, &new_pane);
        self.workspaces[wi].focused_pane = Some(new_pane.clone());
        Ok(new_pane)
    }

    /// Move an existing surface into a **new** pane split off `target_pane` on
    /// `side`. This is what a drop on another pane's edge means: the split lands
    /// where the user dropped the tab, which may be a different pane from the one
    /// holding it. As with [`Self::split_surface`], a target that would hold only
    /// the moved tab has nothing left to divide, so it is a no-op.
    pub fn split_surface_into(
        &mut self,
        surface: &str,
        target_pane: &str,
        side: SplitSide,
    ) -> Result<Ref, MoveError> {
        let (s_wi, _) = self
            .locate_surface(surface)
            .ok_or(MoveError::SurfaceNotFound)?;
        let (t_wi, _) = self.locate_pane(target_pane).ok_or(MoveError::PaneNotFound)?;
        if s_wi != t_wi {
            return Err(MoveError::DifferentWorkspace);
        }
        // Move first, so the split sees the target pane as it will be after the
        // drop — including the case where the source pane empties and collapses.
        self.move_surface(surface, target_pane, None)?;
        self.split_surface(surface, side)
    }

    /// Exchange two panes' positions in the workspace layout. Panes and their
    /// surfaces are untouched; only their places in the split tree change.
    pub fn swap_panes(&mut self, workspace: &str, a: &str, b: &str) -> bool {
        let Some(wi) = self.workspace_index(workspace) else {
            return false;
        };
        let has_a = self.workspaces[wi].panes.iter().any(|p| p.id == a);
        let has_b = self.workspaces[wi].panes.iter().any(|p| p.id == b);
        if !has_a || !has_b || a == b {
            return has_a && has_b;
        }
        swap_layout_panes(&mut self.workspaces[wi].layout, a, b);
        true
    }

    /// Move a surface into a pane of any workspace. Within one workspace this is
    /// [`Self::move_surface`]. Across workspaces the surface keeps its backing
    /// (tmux window / browser target), its old pane collapses when left empty,
    /// and its old workspace is closed when left empty; the closed workspace is
    /// returned beside the target pane.
    pub fn transfer_surface(
        &mut self,
        surface: &str,
        target_pane: &str,
        index: Option<usize>,
    ) -> Result<(Ref, Option<Ref>), MoveError> {
        let (s_wi, _) = self
            .locate_surface(surface)
            .ok_or(MoveError::SurfaceNotFound)?;
        let (t_wi, _) = self.locate_pane(target_pane).ok_or(MoveError::PaneNotFound)?;
        if s_wi == t_wi {
            return self.move_surface(surface, target_pane, index).map(|p| (p, None));
        }
        let item = self
            .surface_cloned(surface)
            .ok_or(MoveError::SurfaceNotFound)?;
        let removed = self.remove_surface(surface);
        // Indices shift when the source workspace closes, so look again.
        let (t_wi, t_pi) = self.locate_pane(target_pane).ok_or(MoveError::PaneNotFound)?;
        let ws = &mut self.workspaces[t_wi];
        let tp = &mut ws.panes[t_pi];
        let len = tp.surfaces.len();
        let at = index.map(|i| i.min(len)).unwrap_or(len);
        tp.surfaces.insert(at, item);
        tp.active_surface = Some(surface.to_string());
        ws.focused_pane = Some(target_pane.to_string());
        Ok((target_pane.to_string(), removed.workspace_closed))
    }

    /// Move a surface out into a new workspace of its own, placed right after
    /// its old one and in the same folder, with the old workspace's cwd. The
    /// surface keeps its backing. Returns `(workspace, pane)`.
    pub fn surface_to_new_workspace(
        &mut self,
        surface: &str,
        title: Option<String>,
    ) -> Result<(Ref, Ref), MoveError> {
        let (s_wi, _) = self
            .locate_surface(surface)
            .ok_or(MoveError::SurfaceNotFound)?;
        let count: usize = self.workspaces[s_wi].panes.iter().map(|p| p.surfaces.len()).sum();
        if count <= 1 {
            return Err(MoveError::OnlySurface);
        }
        let (src_id, folder, cwd) = {
            let w = &self.workspaces[s_wi];
            (w.id.clone(), w.folder.clone(), w.cwd.clone())
        };
        let item = self
            .surface_cloned(surface)
            .ok_or(MoveError::SurfaceNotFound)?;
        self.remove_surface(surface);
        let ws_ref = self.next_workspace();
        let pane_ref = self.next_pane();
        let title = title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| item.title.clone());
        self.add_workspace(Workspace {
            id: ws_ref.clone(),
            title,
            folder,
            cwd,
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane {
                pane: pane_ref.clone(),
            },
            panes: vec![Pane {
                id: pane_ref.clone(),
                surfaces: vec![item],
                active_surface: Some(surface.to_string()),
            }],
            focused_pane: Some(pane_ref.clone()),
            ports: vec![],
            pr: None,
        });
        // Sit right after the workspace it came from.
        if let (Some(from), Some(at)) = (self.workspace_index(&src_id), self.workspace_index(&ws_ref)) {
            let w = self.workspaces.remove(at);
            self.workspaces.insert((from + 1).min(self.workspaces.len()), w);
        }
        Ok((ws_ref, pane_ref))
    }

    // --- notifications / logs --------------------------------------------

    pub fn add_notification(&mut self, mut n: Notification) -> Notification {
        n.id = format!("n{}-{}", n.created_at, self.counters.notification);
        self.counters.notification += 1;
        for w in &mut self.workspaces {
            if Some(&w.id) == n.workspace.as_ref() {
                w.unread_notifications += 1;
            }
        }
        self.notifications.push(n.clone());
        n
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        self.logs.push(entry);
        let over = self.logs.len().saturating_sub(MAX_LOGS);
        if over > 0 {
            self.logs.drain(0..over);
        }
    }

    // --- proto mirror -----------------------------------------------------

    pub fn to_tree(&self) -> Tree {
        Tree {
            host: self.host.clone(),
            folders: self.folders.clone(),
            workspaces: self.workspaces.clone(),
            selected_workspace: self.selected_workspace.clone(),
        }
    }
}

fn split_layout(layout: &mut Layout, target: &str, new_pane: &str, side: SplitSide) -> bool {
    match layout {
        Layout::Pane { pane } if pane == target => {
            let target_sub = Layout::Pane {
                pane: target.to_string(),
            };
            let new_sub = Layout::Pane {
                pane: new_pane.to_string(),
            };
            let direction = match side {
                SplitSide::Left | SplitSide::Right => SplitDirection::Horizontal,
                SplitSide::Up | SplitSide::Down => SplitDirection::Vertical,
            };
            let (first, second) = match side {
                SplitSide::Left | SplitSide::Up => (new_sub, target_sub),
                SplitSide::Right | SplitSide::Down => (target_sub, new_sub),
            };
            *layout = Layout::Split {
                direction,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            };
            true
        }
        Layout::Pane { .. } => false,
        Layout::Split { first, second, .. } => {
            split_layout(first, target, new_pane, side) || split_layout(second, target, new_pane, side)
        }
    }
}

/// Returns `None` when the layout was exactly the removed leaf.
fn remove_pane_layout(layout: Layout, target: &str) -> Option<Layout> {
    match layout {
        Layout::Pane { pane } => {
            if pane == target {
                None
            } else {
                Some(Layout::Pane { pane })
            }
        }
        Layout::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            if let Layout::Pane { pane } = first.as_ref() {
                if pane == target {
                    return Some(*second);
                }
            }
            if let Layout::Pane { pane } = second.as_ref() {
                if pane == target {
                    return Some(*first);
                }
            }
            if layout_contains(&first, target) {
                let f = remove_pane_layout(*first, target)?;
                Some(Layout::Split {
                    direction,
                    ratio,
                    first: Box::new(f),
                    second,
                })
            } else if layout_contains(&second, target) {
                let s = remove_pane_layout(*second, target)?;
                Some(Layout::Split {
                    direction,
                    ratio,
                    first,
                    second: Box::new(s),
                })
            } else {
                Some(Layout::Split {
                    direction,
                    ratio,
                    first,
                    second,
                })
            }
        }
    }
}

pub fn layout_contains(layout: &Layout, pane: &str) -> bool {
    match layout {
        Layout::Pane { pane: p } => p == pane,
        Layout::Split { first, second, .. } => {
            layout_contains(first, pane) || layout_contains(second, pane)
        }
    }
}

/// Swap the two pane refs wherever they appear as layout leaves.
fn swap_layout_panes(layout: &mut Layout, a: &str, b: &str) {
    match layout {
        Layout::Pane { pane } => {
            if pane == a {
                *pane = b.to_string();
            } else if pane == b {
                *pane = a.to_string();
            }
        }
        Layout::Split { first, second, .. } => {
            swap_layout_panes(first, a, b);
            swap_layout_panes(second, a, b);
        }
    }
}

fn set_split_ratio(layout: &mut Layout, pane: &str, ratio: f32) -> bool {
    match layout {
        Layout::Pane { .. } => false,
        Layout::Split {
            ratio: r,
            first,
            second,
            ..
        } => {
            if layout_contains(first, pane) {
                if matches!(first.as_ref(), Layout::Pane{pane: p} if p == pane) {
                    *r = ratio;
                    return true;
                }
                set_split_ratio(first, pane, ratio)
            } else if layout_contains(second, pane) {
                if matches!(second.as_ref(), Layout::Pane{pane: p} if p == pane) {
                    *r = ratio;
                    return true;
                }
                set_split_ratio(second, pane, ratio)
            } else {
                false
            }
        }
    }
}

/// Build a terminal surface with default size.
pub fn terminal_surface(id: Ref, title: impl Into<String>, cols: u16, rows: u16) -> Surface {
    Surface {
        id,
        title: title.into(),
        kind: SurfaceKind::Terminal {
            tmux_pane: None,
            cols,
            rows,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_with_two_panes() -> (SessionState, String) {
        let mut s = SessionState::new("test");
        let ws = s.next_workspace();
        let p1 = s.next_pane();
        let s1 = s.next_surface();
        let p2 = s.next_pane();
        let s2 = s.next_surface();
        let pane1 = Pane {
            id: p1.clone(),
            surfaces: vec![terminal_surface(s1, "a", 80, 24)],
            active_surface: None,
        };
        let pane2 = Pane {
            id: p2.clone(),
            surfaces: vec![terminal_surface(s2, "b", 80, 24)],
            active_surface: None,
        };
        s.add_workspace(Workspace {
            id: ws.clone(),
            title: "w".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: p1.clone() },
            panes: vec![pane1, pane2],
            focused_pane: Some(p1.clone()),
            ports: vec![],
            pr: None,
        });
        (s, ws)
    }

    #[test]
    fn split_right_makes_target_first() {
        let (mut s, ws) = ws_with_two_panes();
        let target = s.workspace(&ws).unwrap().focused_pane.clone().unwrap();
        let newp = s.next_pane();
        assert!(s.split_pane(&ws, &target, SplitSide::Right, &newp));
        let w = s.workspace(&ws).unwrap();
        match &w.layout {
            Layout::Split {
                direction,
                first,
                second,
                ratio,
            } => {
                assert_eq!(*direction, SplitDirection::Horizontal);
                assert_eq!(**first, Layout::Pane { pane: target });
                assert_eq!(**second, Layout::Pane { pane: newp });
                assert_eq!(*ratio, 0.5);
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn split_left_puts_new_first() {
        let (mut s, ws) = ws_with_two_panes();
        let target = s.workspace(&ws).unwrap().focused_pane.clone().unwrap();
        let newp = s.next_pane();
        assert!(s.split_pane(&ws, &target, SplitSide::Left, &newp));
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split {
                direction,
                first,
                second,
                ..
            } => {
                assert_eq!(*direction, SplitDirection::Horizontal);
                assert_eq!(**first, Layout::Pane { pane: newp });
                assert_eq!(**second, Layout::Pane { pane: target });
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn split_up_is_vertical() {
        let (mut s, ws) = ws_with_two_panes();
        let target = s.workspace(&ws).unwrap().focused_pane.clone().unwrap();
        let newp = s.next_pane();
        assert!(s.split_pane(&ws, &target, SplitSide::Up, &newp));
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split { direction, .. } => assert_eq!(*direction, SplitDirection::Vertical),
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn nested_split_then_collapse() {
        let (mut s, ws) = ws_with_two_panes();
        let target = s.workspace(&ws).unwrap().focused_pane.clone().unwrap();
        let newp = s.next_pane();
        s.split_pane(&ws, &target, SplitSide::Right, &newp);
        // now split the new pane downward
        let newp2 = s.next_pane();
        s.workspace_mut(&ws).unwrap().panes.push(Pane {
            id: newp2.clone(),
            surfaces: vec![terminal_surface("surface:99".into(), "c", 80, 24)],
            active_surface: None,
        });
        s.split_pane(&ws, &newp, SplitSide::Down, &newp2);

        // remove newp2 -> collapses back to a single split
        let r = s.remove_surface("surface:99");
        assert_eq!(r.surfaces, vec!["surface:99".to_string()]);
        assert!(r.workspace_closed.is_none());
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split {
                direction,
                first,
                second,
                ..
            } => {
                assert_eq!(*direction, SplitDirection::Horizontal);
                assert_eq!(**first, Layout::Pane { pane: target });
                assert_eq!(**second, Layout::Pane { pane: newp });
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn closing_last_surface_closes_workspace() {
        let (mut s, ws) = ws_with_two_panes();
        let surface = s.workspace(&ws).unwrap().panes[0].surfaces[0].id.clone();
        let r = s.remove_surface(&surface);
        assert_eq!(r.surfaces, vec![surface]);
        assert!(s.workspace(&ws).is_none());
    }

    #[test]
    fn close_one_of_two_panes_keeps_other() {
        let (mut s, ws) = ws_with_two_panes();
        let target = s.workspace(&ws).unwrap().panes[0].id.clone();
        let newp = s.next_pane();
        s.split_pane(&ws, &target, SplitSide::Right, &newp);
        let surface0 = s.workspace(&ws).unwrap().panes[0].surfaces[0].id.clone();
        let r = s.remove_surface(&surface0);
        assert!(r.workspace_closed.is_none());
        let w = s.workspace(&ws).unwrap();
        assert_eq!(w.panes.len(), 1);
        assert_eq!(w.layout, Layout::Pane { pane: newp });
    }

    #[test]
    fn resize_ratio_updates_parent_split() {
        let (mut s, ws) = ws_with_two_panes();
        let target = s.workspace(&ws).unwrap().panes[0].id.clone();
        let newp = s.next_pane();
        s.split_pane(&ws, &target, SplitSide::Right, &newp);
        assert!(s.resize_pane(&ws, &newp, 0.7));
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split { ratio, .. } => assert!((*ratio - 0.7).abs() < 1e-6),
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn add_remove_tab_in_pane() {
        let (mut s, ws) = ws_with_two_panes();
        let pane = s.workspace(&ws).unwrap().panes[0].id.clone();
        let tab = s.next_surface();
        assert!(s.add_surface(&pane, terminal_surface(tab.clone(), "tab2", 80, 24)));
        assert_eq!(s.workspace(&ws).unwrap().panes[0].surfaces.len(), 2);
        let r = s.remove_surface(&tab);
        assert_eq!(r.surfaces, vec![tab]);
        assert_eq!(s.workspace(&ws).unwrap().panes[0].surfaces.len(), 1);
    }

    #[test]
    fn counters_never_reuse_after_observe() {
        let mut s = SessionState::new("h");
        s.observe_ref("surface:42");
        assert_eq!(s.next_surface(), "surface:43");
    }

    #[test]
    fn state_json_round_trip() {
        let (s, _ws) = ws_with_two_panes();
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    #[test]
    fn folder_close_moves_workspaces_to_top() {
        let (mut s, ws) = ws_with_two_panes();
        let f = s.add_folder("proj", None);
        s.workspace_mut(&ws).unwrap().folder = Some(f.id.clone());
        assert!(s.close_folder(&f.id));
        assert!(s.workspace(&ws).unwrap().folder.is_none());
    }

    #[test]
    fn nested_folders_hold_workspaces() {
        let (mut s, ws) = ws_with_two_panes();
        let outer = s.add_folder("webapp", None);
        let inner = s.add_folder("api", Some(outer.id.clone()));
        s.workspace_mut(&ws).unwrap().folder = Some(inner.id.clone());

        assert_eq!(
            s.folder_children(Some(&outer.id)).iter().map(|f| &f.name).collect::<Vec<_>>(),
            vec!["api"]
        );
        assert_eq!(s.folder(&inner.id).unwrap().parent.as_deref(), Some(outer.id.as_str()));
        assert_eq!(s.workspace(&ws).unwrap().folder.as_deref(), Some(inner.id.as_str()));
    }

    #[test]
    fn a_folder_cwd_is_inherited_by_nested_folders_without_one() {
        let mut s = SessionState::new("h");
        let outer = s.add_folder("justcallme", None);
        let inner = s.add_folder("api", Some(outer.id.clone()));
        let plain = s.add_folder("scratch", None);
        assert_eq!(s.folder_cwd(&inner.id), None, "no directory anywhere keeps today's default");
        s.folders.iter_mut().find(|f| f.id == outer.id).unwrap().cwd = Some("/p/justcallme".into());
        assert_eq!(s.folder_cwd(&outer.id).as_deref(), Some("/p/justcallme"));
        assert_eq!(s.folder_cwd(&inner.id).as_deref(), Some("/p/justcallme"));
        s.folders.iter_mut().find(|f| f.id == inner.id).unwrap().cwd = Some("/p/justcallme/api".into());
        assert_eq!(s.folder_cwd(&inner.id).as_deref(), Some("/p/justcallme/api"), "the nearest wins");
        assert_eq!(s.folder_cwd(&plain.id), None);
        assert_eq!(s.folder_cwd("folder:99"), None);
        assert_eq!(outer.host.as_deref(), Some("h"), "the daemon stamps its own host");
    }

    #[test]
    fn folders_resolve_by_ref_number_or_unique_title() {
        let mut s = SessionState::new("h");
        let a = s.add_folder("justcallme", None);
        let b = s.add_folder("dup", None);
        let c = s.add_folder("dup", Some(a.id.clone()));
        assert_eq!(s.resolve_folder(&a.id), Ok(a.id.clone()));
        assert_eq!(s.resolve_folder("1"), Ok(a.id.clone()));
        assert_eq!(s.resolve_folder("justcallme"), Ok(a.id.clone()));
        assert_eq!(s.resolve_folder("dup"), Err(FolderLookupError::Ambiguous(vec![b.id, c.id])));
        assert_eq!(s.resolve_folder("nope"), Err(FolderLookupError::NotFound));
        assert_eq!(s.resolve_folder("folder:99"), Err(FolderLookupError::NotFound));
    }

    #[test]
    fn folder_contents_counts_what_a_close_moves_up() {
        let (mut s, ws) = ws_with_two_panes();
        let outer = s.add_folder("webapp", None);
        s.add_folder("api", Some(outer.id.clone()));
        s.workspace_mut(&ws).unwrap().folder = Some(outer.id.clone());
        assert_eq!(s.folder_contents(&outer.id), (1, 1));
        assert!(s.close_folder(&outer.id));
        assert!(s.folder(&outer.id).is_none());
    }

    #[test]
    fn closing_an_outer_folder_lifts_nested_content_one_level() {
        let (mut s, ws) = ws_with_two_panes();
        let outer = s.add_folder("webapp", None);
        let inner = s.add_folder("api", Some(outer.id.clone()));
        s.workspace_mut(&ws).unwrap().folder = Some(inner.id.clone());

        assert!(s.close_folder(&outer.id));
        // The inner folder survives and becomes top level, keeping its workspace.
        assert_eq!(s.folder(&inner.id).unwrap().parent, None);
        assert_eq!(s.workspace(&ws).unwrap().folder.as_deref(), Some(inner.id.as_str()));
    }

    #[test]
    fn closing_an_inner_folder_lifts_its_workspace_to_its_parent() {
        let (mut s, ws) = ws_with_two_panes();
        let outer = s.add_folder("webapp", None);
        let inner = s.add_folder("api", Some(outer.id.clone()));
        s.workspace_mut(&ws).unwrap().folder = Some(inner.id.clone());

        assert!(s.close_folder(&inner.id));
        assert_eq!(s.workspace(&ws).unwrap().folder.as_deref(), Some(outer.id.as_str()));
        assert!(s.folder_children(Some(&outer.id)).is_empty());
    }

    fn top_level_names(s: &SessionState) -> Vec<String> {
        s.folder_children(None).iter().map(|f| f.name.clone()).collect()
    }

    #[test]
    fn moving_a_folder_into_another_nests_it() {
        let mut s = SessionState::new("t");
        let a = s.add_folder("a", None);
        let b = s.add_folder("b", None);
        assert_eq!(s.move_folder(&b.id, Some(a.id.clone()), None), Ok(()));
        assert_eq!(s.folder(&b.id).unwrap().parent.as_deref(), Some(a.id.as_str()));
        assert_eq!(top_level_names(&s), vec!["a"]);
    }

    #[test]
    fn moving_a_folder_reorders_its_parents_children() {
        let mut s = SessionState::new("t");
        let a = s.add_folder("a", None);
        let b = s.add_folder("b", None);
        let c = s.add_folder("c", None);
        // The slot counts the parent's children in the list without the moved
        // folder, so `Some(0)` is the front and `Some(2)` is the end.
        assert_eq!(s.move_folder(&c.id, None, Some(0)), Ok(()));
        assert_eq!(top_level_names(&s), vec!["c", "a", "b"]);
        assert_eq!(s.move_folder(&a.id, None, Some(2)), Ok(()));
        assert_eq!(top_level_names(&s), vec!["c", "b", "a"]);
        // A slot past the end clamps to the end rather than failing.
        assert_eq!(s.move_folder(&c.id, None, Some(99)), Ok(()));
        assert_eq!(top_level_names(&s), vec!["b", "a", "c"]);
    }

    #[test]
    fn moving_a_folder_to_the_top_level_keeps_it_in_place_when_appended() {
        let mut s = SessionState::new("t");
        let outer = s.add_folder("outer", None);
        let inner = s.add_folder("inner", Some(outer.id.clone()));
        assert_eq!(s.move_folder(&inner.id, None, None), Ok(()));
        assert_eq!(s.folder(&inner.id).unwrap().parent, None);
        // Appending puts it last among the top-level folders, not first.
        assert_eq!(top_level_names(&s), vec!["outer", "inner"]);
    }

    #[test]
    fn a_folder_cannot_move_into_itself_or_a_descendant() {
        let mut s = SessionState::new("t");
        let outer = s.add_folder("outer", None);
        let inner = s.add_folder("inner", Some(outer.id.clone()));
        assert_eq!(s.move_folder(&outer.id, Some(outer.id.clone()), None), Err(MoveFolderError::Cycle));
        assert_eq!(s.move_folder(&outer.id, Some(inner.id.clone()), None), Err(MoveFolderError::Cycle));
        // The refused moves changed nothing.
        assert_eq!(s.folder(&inner.id).unwrap().parent.as_deref(), Some(outer.id.as_str()));
        assert_eq!(top_level_names(&s), vec!["outer"]);
    }

    #[test]
    fn moving_an_unknown_folder_or_into_an_unknown_parent_fails() {
        let mut s = SessionState::new("t");
        let a = s.add_folder("a", None);
        assert_eq!(s.move_folder("folder:404", None, None), Err(MoveFolderError::NotFound));
        assert_eq!(
            s.move_folder(&a.id, Some("folder:404".into()), None),
            Err(MoveFolderError::ParentNotFound)
        );
        assert_eq!(top_level_names(&s), vec!["a"]);
    }

    // --- drag-and-drop rearranging ---------------------------------------

    /// A workspace with `n` panes (right-leaning split), each one terminal surface.
    /// Returns the state, workspace ref, and (pane, surface) refs in order.
    fn make(n: usize) -> (SessionState, String, Vec<(Ref, Ref)>) {
        let mut s = SessionState::new("t");
        let ws_ref = s.next_workspace();
        let mut refs = Vec::new();
        let mut panes = Vec::new();
        for i in 0..n {
            let p = s.next_pane();
            let surf = s.next_surface();
            let mut surface = terminal_surface(surf.clone(), "t", 80, 24);
            if let SurfaceKind::Terminal { tmux_pane, .. } = &mut surface.kind {
                *tmux_pane = Some(format!("%{i}"));
            }
            panes.push(Pane {
                id: p.clone(),
                surfaces: vec![surface],
                active_surface: Some(surf.clone()),
            });
            refs.push((p, surf));
        }
        let mut layout = Layout::Pane {
            pane: refs[0].0.clone(),
        };
        for (p, _) in refs.iter().skip(1) {
            layout = Layout::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(layout),
                second: Box::new(Layout::Pane { pane: p.clone() }),
            };
        }
        let first = refs[0].0.clone();
        s.add_workspace(Workspace {
            id: ws_ref.clone(),
            title: "w".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout,
            panes,
            focused_pane: Some(first),
            ports: vec![],
            pr: None,
        });
        (s, ws_ref, refs)
    }

    fn surface_order(s: &SessionState, pane: &str) -> Vec<Ref> {
        let (wi, pi) = s.locate_pane(pane).unwrap();
        s.workspaces[wi].panes[pi]
            .surfaces
            .iter()
            .map(|s| s.id.clone())
            .collect()
    }

    #[test]
    fn reorder_within_pane_by_index() {
        let (mut s, _ws, refs) = make(1);
        let pane = refs[0].0.clone();
        let a = refs[0].1.clone();
        let b = s.next_surface();
        let c = s.next_surface();
        s.add_surface(&pane, terminal_surface(b.clone(), "b", 80, 24));
        s.add_surface(&pane, terminal_surface(c.clone(), "c", 80, 24));
        assert_eq!(surface_order(&s, &pane), vec![a.clone(), b.clone(), c.clone()]);

        // move c to the front
        assert_eq!(s.move_surface(&c, &pane, Some(0)).unwrap(), pane);
        assert_eq!(surface_order(&s, &pane), vec![c.clone(), a.clone(), b.clone()]);

        // move c to the end (default index)
        assert_eq!(s.move_surface(&c, &pane, None).unwrap(), pane);
        assert_eq!(surface_order(&s, &pane), vec![a.clone(), b.clone(), c.clone()]);
    }

    #[test]
    fn reorder_index_is_clamped() {
        let (mut s, _ws, refs) = make(1);
        let pane = refs[0].0.clone();
        let a = refs[0].1.clone();
        let b = s.next_surface();
        s.add_surface(&pane, terminal_surface(b.clone(), "b", 80, 24));
        // 99 clamps to the end
        s.move_surface(&a, &pane, Some(99)).unwrap();
        assert_eq!(surface_order(&s, &pane), vec![b.clone(), a.clone()]);
    }

    #[test]
    fn move_surface_to_another_pane() {
        let (mut s, _ws, refs) = make(2);
        let (p0, s0) = refs[0].clone();
        let (p1, s1) = refs[1].clone();
        assert_eq!(s.move_surface(&s0, &p1, Some(0)).unwrap(), p1);
        // target has the moved surface first, source pane is gone
        assert_eq!(surface_order(&s, &p1), vec![s0.clone(), s1.clone()]);
        assert!(s.locate_pane(&p0).is_none());
        assert_eq!(s.workspace("workspace:1").unwrap().layout, Layout::Pane { pane: p1.clone() });
        // the moved surface keeps its backing
        if let SurfaceKind::Terminal { tmux_pane, .. } = &s.surface(&s0).unwrap().kind {
            assert_eq!(tmux_pane.as_deref(), Some("%0"));
        } else {
            panic!("expected terminal");
        }
    }

    #[test]
    fn move_last_chip_collapses_nested_split() {
        let (mut s, ws, refs) = make(3);
        let (p0, s0) = refs[0].clone();
        let (p1, s1) = refs[1].clone();
        let (p2, _s2) = refs[2].clone();
        // move s1 (the middle pane's only surface) into p0 -> p1 disappears
        s.move_surface(&s1, &p0, None).unwrap();
        let w = s.workspace(&ws).unwrap();
        assert_eq!(w.panes.len(), 2);
        assert!(w.panes.iter().all(|p| p.id != p1));
        assert!(layout_contains(&w.layout, &p0));
        assert!(layout_contains(&w.layout, &p2));
        assert!(!layout_contains(&w.layout, &p1));
        assert!(s.locate_surface(&s1).is_some());
        assert!(s.surface(&s0).is_some());
    }

    #[test]
    fn move_surface_errors() {
        let (mut s, _ws, refs) = make(2);
        let (p0, s0) = refs[0].clone();
        assert_eq!(
            s.move_surface("surface:999", &p0, None),
            Err(MoveError::SurfaceNotFound)
        );
        assert_eq!(
            s.move_surface(&s0, "pane:999", None),
            Err(MoveError::PaneNotFound)
        );
        // different workspaces
        let mut two = s.clone();
        let ws2 = two.next_workspace();
        let p = two.next_pane();
        let surf = two.next_surface();
        two.add_workspace(Workspace {
            id: ws2.clone(),
            title: "w2".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: p.clone() },
            panes: vec![Pane {
                id: p.clone(),
                surfaces: vec![terminal_surface(surf, "t", 80, 24)],
                active_surface: None,
            }],
            focused_pane: Some(p.clone()),
            ports: vec![],
            pr: None,
        });
        assert_eq!(
            two.move_surface(&s0, &p, None),
            Err(MoveError::DifferentWorkspace)
        );
    }

    #[test]
    fn split_surface_moves_to_new_pane() {
        let (mut s, ws, refs) = make(1);
        let pane = refs[0].0.clone();
        let a = refs[0].1.clone();
        let b = s.next_surface();
        s.add_surface(&pane, terminal_surface(b.clone(), "b", 80, 24));

        // split b out to the right
        let new_pane = s.split_surface(&b, SplitSide::Right).unwrap();
        assert_ne!(new_pane, pane);
        assert_eq!(surface_order(&s, &pane), vec![a.clone()]);
        assert_eq!(surface_order(&s, &new_pane), vec![b.clone()]);
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split {
                direction,
                first,
                second,
                ..
            } => {
                assert_eq!(*direction, SplitDirection::Horizontal);
                assert_eq!(**first, Layout::Pane { pane: pane.clone() });
                assert_eq!(**second, Layout::Pane { pane: new_pane.clone() });
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn split_surface_left_puts_new_pane_first() {
        let (mut s, ws, refs) = make(1);
        let pane = refs[0].0.clone();
        let a = refs[0].1.clone();
        let b = s.next_surface();
        s.add_surface(&pane, terminal_surface(b.clone(), "b", 80, 24));
        let new_pane = s.split_surface(&b, SplitSide::Left).unwrap();
        assert_eq!(surface_order(&s, &pane), vec![a]);
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split { first, second, .. } => {
                assert_eq!(**first, Layout::Pane { pane: new_pane.clone() });
                assert_eq!(**second, Layout::Pane { pane: pane.clone() });
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn split_surface_single_chip_is_noop() {
        let (mut s, ws, refs) = make(1);
        let pane = refs[0].0.clone();
        let a = refs[0].1.clone();
        let before = s.workspace(&ws).unwrap().layout.clone();
        let out = s.split_surface(&a, SplitSide::Down).unwrap();
        assert_eq!(out, pane);
        assert_eq!(s.workspace(&ws).unwrap().layout, before);
        assert_eq!(s.workspace(&ws).unwrap().panes.len(), 1);
        assert_eq!(surface_order(&s, &pane), vec![a]);
    }

    #[test]
    fn split_surface_into_lands_on_the_target_pane() {
        let (mut s, ws, refs) = make(2);
        let (p0, a) = refs[0].clone();
        let (p1, _b) = refs[1].clone();

        // `a` lives in p0 and is dropped on p1's top edge.
        let new_pane = s.split_surface_into(&a, &p1, SplitSide::Up).unwrap();
        assert_ne!(new_pane, p0);
        assert_eq!(surface_order(&s, &new_pane), vec![a.clone()]);
        // p0 emptied, so it is gone; only the split of p1 remains.
        assert!(s.locate_pane(&p0).is_none(), "the emptied source pane is removed");
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split { direction, first, second, .. } => {
                assert_eq!(*direction, SplitDirection::Vertical);
                assert_eq!(**first, Layout::Pane { pane: new_pane.clone() });
                assert_eq!(**second, Layout::Pane { pane: p1.clone() });
            }
            other => panic!("expected the target pane to split, got {other:?}"),
        }
    }

    #[test]
    fn split_surface_into_single_tab_target_is_noop() {
        let (mut s, ws, refs) = make(1);
        let pane = refs[0].0.clone();
        let a = refs[0].1.clone();
        let before = s.workspace(&ws).unwrap().layout.clone();
        // The only tab in the pane cannot be split off the pane it is already in.
        assert_eq!(s.split_surface_into(&a, &pane, SplitSide::Down).unwrap(), pane);
        assert_eq!(s.workspace(&ws).unwrap().layout, before);
        assert_eq!(surface_order(&s, &pane), vec![a]);
    }

    #[test]
    fn swap_panes_flips_layout() {
        let (mut s, ws, refs) = make(2);
        let (p0, _) = refs[0].clone();
        let (p1, _) = refs[1].clone();
        assert!(s.swap_panes(&ws, &p0, &p1));
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split { first, second, .. } => {
                assert_eq!(**first, Layout::Pane { pane: p1.clone() });
                assert_eq!(**second, Layout::Pane { pane: p0.clone() });
            }
            other => panic!("expected split, got {other:?}"),
        }
        // swapping back restores
        assert!(s.swap_panes(&ws, &p0, &p1));
        match &s.workspace(&ws).unwrap().layout {
            Layout::Split { first, second, .. } => {
                assert_eq!(**first, Layout::Pane { pane: p0 });
                assert_eq!(**second, Layout::Pane { pane: p1 });
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn swap_panes_missing_returns_false() {
        let (mut s, ws, refs) = make(2);
        assert!(!s.swap_panes(&ws, &refs[0].0, "pane:999"));
        assert!(!s.swap_panes("workspace:999", &refs[0].0, &refs[1].0));
    }

    #[test]
    fn pane_index_tracks_terms() {
        let mut s = SessionState::new("t");
        s.set_term("surface:1", "@1", "%1");
        assert_eq!(s.surface_of_pane("%1").as_deref(), Some("surface:1"));
        assert_eq!(s.surface_of_window("@1").as_deref(), Some("surface:1"));
        let meta = s.remove_term("surface:1").unwrap();
        assert_eq!(meta.pane, "%1");
        assert!(s.surface_of_pane("%1").is_none());
        assert!(s.surface_of_window("@1").is_none());
        // remove_window removes both directions too.
        s.set_term("surface:2", "@2", "%2");
        assert_eq!(s.remove_window("@2").as_deref(), Some("surface:2"));
        assert!(s.surface_of_pane("%2").is_none());
        assert!(s.term("surface:2").is_none());
    }

    #[test]
    fn nested_swap_only_touches_named_panes() {
        let (mut s, ws, refs) = make(3);
        let (p0, _) = refs[0].clone();
        let (p1, _) = refs[1].clone();
        let (p2, _) = refs[2].clone();
        assert!(s.swap_panes(&ws, &p1, &p2));
        let w = s.workspace(&ws).unwrap();
        // p1 and p2 exchanged places; p0 unchanged.
        assert!(layout_contains(&w.layout, &p0));
        let l = &w.layout;
        // right-leaning: outer split first is inner split [p0,p1], second was p2.
        match l {
            Layout::Split { first, second, .. } => {
                assert_eq!(**second, Layout::Pane { pane: p1.clone() });
                assert!(layout_contains(first, &p2));
                assert!(layout_contains(first, &p0));
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    /// Add a one-pane, one-surface workspace; returns (workspace, pane, surface).
    fn add_single(s: &mut SessionState) -> (Ref, Ref, Ref) {
        let ws = s.next_workspace();
        let p = s.next_pane();
        let surf = s.next_surface();
        s.add_workspace(Workspace {
            id: ws.clone(),
            title: "other".into(),
            folder: None,
            cwd: "/var".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: p.clone() },
            panes: vec![Pane {
                id: p.clone(),
                surfaces: vec![terminal_surface(surf.clone(), "o", 80, 24)],
                active_surface: Some(surf.clone()),
            }],
            focused_pane: Some(p.clone()),
            ports: vec![],
            pr: None,
        });
        (ws, p, surf)
    }

    #[test]
    fn transfer_surface_across_workspaces() {
        let (mut s, ws, refs) = make(2);
        let (ws2, p2, _) = add_single(&mut s);
        let (_, moving) = refs[1].clone();
        let (pane, closed) = s.transfer_surface(&moving, &p2, Some(0)).unwrap();
        assert_eq!(pane, p2);
        assert!(closed.is_none());
        assert_eq!(s.workspace_of_surface(&moving).as_deref(), Some(ws2.as_str()));
        let w2 = s.workspace(&ws2).unwrap();
        assert_eq!(w2.panes[0].surfaces[0].id, moving, "inserted at index 0");
        assert_eq!(w2.panes[0].active_surface.as_deref(), Some(moving.as_str()));
        // The source pane emptied and collapsed out of the layout.
        let w = s.workspace(&ws).unwrap();
        assert_eq!(w.panes.len(), 1);
        assert!(!layout_contains(&w.layout, &refs[1].0));

        // Moving the last surface of a workspace away closes it.
        let (_, last) = refs[0].clone();
        let (_, closed) = s.transfer_surface(&last, &p2, None).unwrap();
        assert_eq!(closed.as_deref(), Some(ws.as_str()));
        assert!(s.workspace(&ws).is_none());
        assert_eq!(s.workspace(&ws2).unwrap().panes[0].surfaces.len(), 3);
    }

    #[test]
    fn surface_to_new_workspace_moves_and_places_after_source() {
        let (mut s, ws, refs) = make(2);
        let (ws_other, _, _) = add_single(&mut s);
        let (_, moving) = refs[1].clone();
        let (new_ws, new_pane) = s
            .surface_to_new_workspace(&moving, Some("logs".into()))
            .unwrap();
        let order: Vec<&str> = s.workspaces.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(order, vec![ws.as_str(), new_ws.as_str(), ws_other.as_str()]);
        let w = s.workspace(&new_ws).unwrap();
        assert_eq!(w.title, "logs");
        assert_eq!(w.cwd, "/tmp", "inherits the source workspace's cwd");
        assert_eq!(w.layout, Layout::Pane { pane: new_pane.clone() });
        assert_eq!(s.pane_of_surface(&moving).as_deref(), Some(new_pane.as_str()));
        assert_eq!(s.workspace(&ws).unwrap().panes.len(), 1);

        // The only surface of a workspace has nowhere to go.
        assert_eq!(
            s.surface_to_new_workspace(&moving, None),
            Err(MoveError::OnlySurface)
        );
        assert_eq!(
            s.surface_to_new_workspace("surface:99", None),
            Err(MoveError::SurfaceNotFound)
        );
    }
}
