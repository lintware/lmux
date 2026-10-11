//! RPC dispatch over the shared `lmux_proto::methods` names.

use std::sync::Arc;

use lmux_proto::*;
use serde_json::{json, Value};

use crate::browser::unsupported;
use crate::daemon::{Inner, Out};
use crate::tree::{terminal_surface, MoveFolderError};

fn ok(id: u64, v: Value) -> RpcResponse {
    RpcResponse {
        id,
        result: Some(v),
        error: None,
    }
}

fn err(id: u64, code: RpcErrorCode, msg: impl Into<String>) -> RpcResponse {
    RpcResponse {
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: msg.into(),
        }),
    }
}

fn s(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}
fn v_str(params: &Value, key: &str) -> Option<String> {
    match params.get(key)? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}
fn u(params: &Value, key: &str) -> Option<u64> {
    params.get(key).and_then(|v| v.as_u64())
}
fn f(params: &Value, key: &str) -> Option<f32> {
    params.get(key).and_then(|v| v.as_f64()).map(|x| x as f32)
}
fn b(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(|v| v.as_bool())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_side(params: &Value) -> SplitSide {
    match s(params, "side").as_deref() {
        Some("left") => SplitSide::Left,
        Some("up") => SplitSide::Up,
        Some("down") => SplitSide::Down,
        _ => SplitSide::Right,
    }
}

fn move_err(e: crate::tree::MoveError) -> RpcError {
    use crate::tree::MoveError::*;
    match e {
        SurfaceNotFound => RpcError {
            code: RpcErrorCode::NotFound,
            message: "surface not found".into(),
        },
        PaneNotFound => RpcError {
            code: RpcErrorCode::NotFound,
            message: "pane not found".into(),
        },
        DifferentWorkspace => RpcError {
            code: RpcErrorCode::InvalidParams,
            message: "surface and pane are in different workspaces".into(),
        },
        OnlySurface => RpcError {
            code: RpcErrorCode::InvalidParams,
            message: "the surface is the only one in its workspace".into(),
        },
    }
}

fn not_found(msg: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcErrorCode::NotFound,
        message: msg.into(),
    }
}

/// Where `surface.move` puts a surface: `(pane, index)`. `before`/`after` name
/// a tab to sit next to; else `pane`; else `workspace`'s focused pane; else the
/// surface's own pane (a reorder).
fn move_target(
    state: &crate::tree::SessionState,
    surface: &str,
    params: &Value,
) -> Result<(Ref, Option<usize>), RpcError> {
    let anchor = s(params, "before")
        .map(|a| (a, 0))
        .or_else(|| s(params, "after").map(|a| (a, 1)));
    if let Some((a, after)) = anchor {
        let pane = state
            .pane_of_surface(&a)
            .ok_or_else(|| not_found(format!("surface {a} not found")))?;
        let (wi, pi) = state.locate_pane(&pane).ok_or_else(|| not_found("pane not found"))?;
        // Positions as they will be once the moving tab has left.
        let pos = state.workspaces[wi].panes[pi]
            .surfaces
            .iter()
            .filter(|x| x.id != surface)
            .position(|x| x.id == a)
            .ok_or_else(|| not_found(format!("surface {a} not found")))?;
        return Ok((pane, Some(pos + after)));
    }
    let index = u(params, "index").map(|n| n as usize);
    if let Some(p) = s(params, "pane") {
        return Ok((p, index));
    }
    if let Some(ws) = s(params, "workspace") {
        let w = state
            .workspace(&ws)
            .ok_or_else(|| not_found(format!("workspace {ws} not found")))?;
        let pane = w
            .focused_pane
            .clone()
            .or_else(|| w.panes.first().map(|p| p.id.clone()))
            .ok_or_else(|| not_found("workspace has no pane"))?;
        return Ok((pane, index));
    }
    let own = state
        .pane_of_surface(surface)
        .ok_or_else(|| not_found("surface not found"))?;
    Ok((own, index))
}

fn resolve_workspace(state: &crate::tree::SessionState, params: &Value) -> Option<Ref> {
    // An explicit target that does not exist is an error, not a fallback to the
    // caller: returning None makes the caller answer NotFound.
    if let Some(w) = s(params, "workspace") {
        return if state.workspace(&w).is_some() {
            Some(w)
        } else {
            None
        };
    }
    // The caller's surface wins over its workspace env var: a tab moved to
    // another workspace keeps the `LMUX_WORKSPACE_ID` it was started with.
    if let Some(surf) = s(params, "caller_surface") {
        if let Some(w) = state.workspace_of_surface(&surf) {
            return Some(w);
        }
    }
    if let Some(w) = s(params, "caller_workspace") {
        if state.workspace(&w).is_some() {
            return Some(w);
        }
    }
    if let Some(sel) = &state.selected_workspace {
        if state.workspace(sel).is_some() {
            return Some(sel.clone());
        }
    }
    state.workspaces.first().map(|w| w.id.clone())
}

fn resolve_surface(state: &crate::tree::SessionState, params: &Value) -> Option<Ref> {
    if let Some(surf) = s(params, "surface") {
        return if state.surface(&surf).is_some() {
            Some(surf)
        } else {
            None
        };
    }
    // An explicit workspace means its focused surface, not the caller's.
    if s(params, "workspace").is_none() {
        if let Some(surf) = s(params, "caller_surface") {
            if state.surface(&surf).is_some() {
                return Some(surf);
            }
        }
    }
    let ws = resolve_workspace(state, params)?;
    state.focused_surface(&ws)
}

/// The surface a `browser.*` call acts on. An explicit `surface` is used as
/// is. Otherwise the caller is usually an agent in a terminal, so a resolved
/// terminal gives way to a browser in the same workspace: the focused pane's
/// active browser, else the active browser of any pane, else the newest one.
fn resolve_browser_surface(state: &crate::tree::SessionState, params: &Value) -> Option<Ref> {
    let surf = resolve_surface(state, params);
    if s(params, "surface").is_some() {
        return surf;
    }
    if let Some(surf) = &surf {
        if surface_is_browser(state, surf) {
            return Some(surf.clone());
        }
    }
    let ws = surf
        .as_ref()
        .and_then(|x| state.workspace_of_surface(x))
        .or_else(|| resolve_workspace(state, params));
    let Some(w) = ws.as_ref().and_then(|ws| state.workspace(ws)) else {
        return surf;
    };
    let is_browser = |id: &str| surface_is_browser(state, id);
    let focused = w
        .focused_pane
        .as_ref()
        .and_then(|p| w.panes.iter().find(|x| &x.id == p))
        .and_then(|p| p.active_surface.clone())
        .filter(|x| is_browser(x));
    let active = || {
        w.panes
            .iter()
            .filter_map(|p| p.active_surface.clone())
            .filter(|x| is_browser(x))
            .last()
    };
    let newest = || {
        w.panes
            .iter()
            .flat_map(|p| p.surfaces.iter())
            .filter(|x| matches!(x.kind, SurfaceKind::Browser { .. }))
            .max_by_key(|x| ref_number(&x.id))
            .map(|x| x.id.clone())
    };
    focused.or_else(active).or_else(newest).or(surf)
}

/// Which notifications a clear / mark-read acts on.
#[derive(Debug, PartialEq)]
enum NotifySelector {
    Id(String),
    All,
    /// Only those already read (`dismiss-notification --all-read`).
    Read,
    Workspace { workspace: Ref, surface: Option<Ref> },
}

impl NotifySelector {
    /// `id`, then the `all_key` flag (`all` or `all_read`), then a workspace:
    /// the explicit one, else the caller's. With none of those, all of them.
    fn from_params(state: &crate::tree::SessionState, params: &Value, all_key: &str) -> Self {
        if let Some(nid) = s(params, "id") {
            return NotifySelector::Id(nid);
        }
        if b(params, all_key).unwrap_or(false) {
            return if all_key == "all_read" { NotifySelector::Read } else { NotifySelector::All };
        }
        let ws = s(params, "workspace")
            .or_else(|| s(params, "caller_surface").and_then(|x| state.workspace_of_surface(&x)))
            .or_else(|| s(params, "caller_workspace"));
        match ws {
            Some(workspace) => NotifySelector::Workspace {
                workspace,
                surface: s(params, "surface"),
            },
            None => NotifySelector::All,
        }
    }

    fn matches(&self, n: &Notification) -> bool {
        match self {
            NotifySelector::Id(id) => &n.id == id,
            NotifySelector::All => true,
            NotifySelector::Read => n.read,
            NotifySelector::Workspace { workspace, surface } => {
                n.workspace.as_deref() == Some(workspace.as_str())
                    && surface.as_ref().is_none_or(|x| n.surface.as_deref() == Some(x.as_str()))
            }
        }
    }
}

/// Recompute every workspace's unread badge from the notification list.
fn recount_unread(state: &mut crate::tree::SessionState) {
    let mut counts: std::collections::HashMap<Ref, u32> = std::collections::HashMap::new();
    for n in state.notifications.iter().filter(|n| !n.read) {
        if let Some(w) = &n.workspace {
            *counts.entry(w.clone()).or_default() += 1;
        }
    }
    for w in &mut state.workspaces {
        w.unread_notifications = counts.get(&w.id).copied().unwrap_or(0);
    }
}

/// A fresh browser surface. Its page is created by the backend, or by the app
/// when the browser is drawn there (`in_app`).
pub(crate) fn browser_surface(id: Ref, url: Option<&str>, in_app: bool) -> Surface {
    Surface {
        id,
        title: url.unwrap_or("browser").to_string(),
        kind: SurfaceKind::Browser {
            url: url.unwrap_or("about:blank").to_string(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            in_app,
        },
    }
}

/// Browser panes are drawn and run by the app here, not by a Chromium.
fn browsers_in_app(inner: &Inner) -> bool {
    inner.browser.is_none() && inner.cfg.app_browser
}

/// Make the page for a new browser surface: in Chromium, or nothing to do when
/// the app draws it. On failure the surface is taken back out.
async fn create_page(inner: &Inner, surface: &str, url: Option<&str>) -> Result<(), String> {
    let Some(backend) = &inner.browser else {
        return if inner.cfg.app_browser { Ok(()) } else { Err("browser backend not available".into()) };
    };
    match backend.create_surface(surface, url).await {
        Ok(()) => Ok(()),
        Err(e) => {
            inner.state.lock().await.remove_surface(surface);
            Err(format!("browser: {e}"))
        }
    }
}

/// A workspace's focus: its focused pane and every pane's active tab.
type FocusSnapshot = (Ref, Option<Ref>, Vec<(Ref, Option<Ref>)>);

fn focus_snapshot(state: &crate::tree::SessionState, ws: &str) -> Option<FocusSnapshot> {
    let w = state.workspace(ws)?;
    Some((
        ws.to_string(),
        w.focused_pane.clone(),
        w.panes.iter().map(|p| (p.id.clone(), p.active_surface.clone())).collect(),
    ))
}

/// Put back a [`focus_snapshot`], for creates asked to leave focus alone.
fn restore_focus(state: &mut crate::tree::SessionState, snap: FocusSnapshot) {
    let (ws, focused, actives) = snap;
    let Some(w) = state.workspace_mut(&ws) else { return };
    if focused.as_ref().is_some_and(|p| w.panes.iter().any(|x| &x.id == p)) {
        w.focused_pane = focused;
    }
    for (pane, active) in actives {
        if let Some(p) = w.panes.iter_mut().find(|p| p.id == pane) {
            if active.as_ref().is_some_and(|a| p.surfaces.iter().any(|x| &x.id == a)) {
                p.active_surface = active;
            }
        }
    }
}

/// Re-tag a terminal's tmux window with its workspace after a move, so a
/// daemon that lost its state file adopts it into the right one.
async fn retag_workspace(inner: &Arc<Inner>, surface: &str) {
    let (term, ws) = {
        let state = inner.state.lock().await;
        (state.term_cloned(surface), state.workspace_of_surface(surface))
    };
    if let (Some(term), Some(ws)) = (term, ws) {
        let _ = inner.tmux.set_window_option(&term.window, "@lmux_workspace", &ws).await;
    }
}

/// The number in a ref like `surface:12`, for "newest" ordering.
fn ref_number(r: &str) -> u64 {
    r.rsplit(':').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

fn resolve_pane(state: &crate::tree::SessionState, params: &Value) -> Option<Ref> {
    if let Some(p) = s(params, "pane") {
        return if state.locate_pane(&p).is_some() {
            Some(p)
        } else {
            None
        };
    }
    if let Some(surf) = resolve_surface(state, params) {
        if let Some(p) = state.pane_of_surface(&surf) {
            return Some(p);
        }
    }
    let ws = resolve_workspace(state, params)?;
    let w = state.workspace(&ws)?;
    w.focused_pane
        .clone()
        .or_else(|| w.panes.first().map(|p| p.id.clone()))
}

fn surface_is_browser(state: &crate::tree::SessionState, surface: &str) -> bool {
    matches!(
        state.surface(surface),
        Some(Surface {
            kind: SurfaceKind::Browser { .. },
            ..
        })
    )
}

/// Where the terminal shown in `pane` is now (tmux `pane_current_path`), so a
/// new tab or split opens beside it rather than back at the workspace's start.
async fn live_cwd(inner: &Arc<Inner>, pane: &str) -> Option<String> {
    let tmux_pane = {
        let state = inner.state.lock().await;
        let (wi, pi) = state.locate_pane(pane)?;
        let p = &state.workspaces[wi].panes[pi];
        p.active_surface
            .iter()
            .chain(p.surfaces.iter().map(|s| &s.id))
            .find_map(|s| state.term_cloned(s))?
            .pane
    };
    let cwd = inner.tmux.display(&tmux_pane, "#{pane_current_path}").await.ok()?;
    let cwd = cwd.trim();
    (!cwd.is_empty()).then(|| cwd.to_owned())
}

fn default_cwd(params: &Value, state: &crate::tree::SessionState, ws: Option<&str>) -> String {
    if let Some(c) = s(params, "cwd") {
        return c;
    }
    if let Some(ws) = ws {
        if let Some(w) = state.workspace(ws) {
            if !w.cwd.is_empty() {
                return w.cwd.clone();
            }
        }
    }
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

/// Shells count as idle: closing a pane at its prompt loses nothing.
const SHELLS: &[&str] = &["zsh", "bash", "sh", "fish", "dash", "ksh", "tcsh", "csh", "nu", "login", "-zsh", "-bash"];

/// The first program other than a shell running in the foreground of one of
/// these terminal surfaces, asked of tmux now, so a close can be confirmed.
async fn busy_program(inner: &Arc<Inner>, surfaces: &[String]) -> Option<String> {
    let panes: Vec<String> = {
        let state = inner.state.lock().await;
        surfaces.iter().filter_map(|s| state.term_cloned(s)).map(|t| t.pane).collect()
    };
    for pane in panes {
        let Ok(cmd) = inner.tmux.display(&pane, "#{pane_current_command}").await else { continue };
        let cmd = cmd.trim();
        if !cmd.is_empty() && !SHELLS.contains(&cmd) {
            return Some(cmd.to_owned());
        }
    }
    None
}

pub(crate) async fn spawn_terminal(
    inner: &Arc<Inner>,
    workspace: &str,
    surface: &str,
    title: &str,
    cwd: &str,
    command: Option<&str>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<(String, String)> {
    spawn_terminal_env(inner, workspace, surface, title, cwd, command, &[], cols, rows).await
}

/// `spawn_terminal` with extra environment for the window's first process.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_terminal_env(
    inner: &Arc<Inner>,
    workspace: &str,
    surface: &str,
    title: &str,
    cwd: &str,
    command: Option<&str>,
    extra_env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> anyhow::Result<(String, String)> {
    let mut env = vec![
        (
            "LMUX_SOCKET".to_string(),
            inner.cfg.socket_path().to_string_lossy().to_string(),
        ),
        ("LMUX_WORKSPACE_ID".to_string(), workspace.to_string()),
        ("LMUX_SURFACE_ID".to_string(), surface.to_string()),
    ];
    env.extend(extra_env.iter().cloned());
    let (window, pane) = inner
        .tmux
        .new_terminal(cwd, command, &env, cols, rows, title, surface, workspace)
        .await?;
    {
        let mut state = inner.state.lock().await;
        state.set_term(surface, &window, &pane);
        state.set_surface_tmux(surface, &pane);
    }
    Ok((window, pane))
}

/// Turn the target params people type into refs: a bare number names that
/// ref (`2` is `workspace:2`), and a workspace may also be named by its title.
/// An explicit workspace that matches nothing is an error here, so it is not
/// reported later as a missing surface or pane.
fn normalize_targets(state: &crate::tree::SessionState, params: &mut Value) -> Result<(), RpcError> {
    let Some(obj) = params.as_object_mut() else {
        return Ok(());
    };
    for key in ["surface", "pane"] {
        if let Some(Value::String(v)) = obj.get_mut(key) {
            if !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()) {
                *v = format!("{key}:{v}");
            }
        }
    }
    let Some(Value::String(ws)) = obj.get("workspace") else {
        return Ok(());
    };
    if state.workspace(ws).is_some() {
        return Ok(());
    }
    let numbered = format!("workspace:{ws}");
    let found = if !ws.is_empty() && ws.bytes().all(|c| c.is_ascii_digit()) && state.workspace(&numbered).is_some() {
        numbered
    } else {
        let named: Vec<&Ref> = state.workspaces.iter().filter(|w| &w.title == ws).map(|w| &w.id).collect();
        match named.as_slice() {
            [one] => (*one).clone(),
            [] => return Err(not_found(format!("workspace {ws} not found"))),
            many => {
                return Err(RpcError {
                    code: RpcErrorCode::InvalidParams,
                    message: format!("{} workspaces are named {ws:?}; use a ref like {}", many.len(), many[0]),
                })
            }
        }
    };
    obj.insert("workspace".into(), Value::String(found));
    Ok(())
}

pub async fn dispatch(inner: &Arc<Inner>, req: &RpcRequest) -> RpcResponse {
    let mut params = req.params.clone();
    if let Err(e) = normalize_targets(&*inner.state.lock().await, &mut params) {
        return RpcResponse { id: req.id, result: None, error: Some(e) };
    }
    let req = &RpcRequest { id: req.id, method: req.method.clone(), params };
    // Creates move focus to what they make, unless asked not to (`focus: false`,
    // cmux's `--focus false`): then the workspace's focus is put back after.
    let creates = [methods::SPLIT_NEW, methods::SURFACE_NEW, methods::BROWSER_OPEN];
    let snap = if b(&req.params, "focus") == Some(false) && creates.contains(&req.method.as_str()) {
        let state = inner.state.lock().await;
        resolve_pane(&state, &req.params)
            .and_then(|p| state.locate_pane(&p).map(|(wi, _)| state.workspaces[wi].id.clone()))
            .or_else(|| resolve_workspace(&state, &req.params))
            .and_then(|ws| focus_snapshot(&state, &ws))
    } else {
        None
    };
    let resp = dispatch_inner(inner, req).await;
    if let (Some(snap), None) = (snap, &resp.error) {
        restore_focus(&mut *inner.state.lock().await, snap);
        inner.tree_changed();
    }
    resp
}

async fn dispatch_inner(inner: &Arc<Inner>, req: &RpcRequest) -> RpcResponse {
    let id = req.id;
    let params = &req.params;
    match req.method.as_str() {
        methods::PING => ok(id, json!({"pong": true})),

        methods::IDENTIFY => {
            let state = inner.state.lock().await;
            let surface = resolve_surface(&state, params);
            let pane = surface.as_ref().and_then(|s| state.pane_of_surface(s));
            let workspace = surface.as_ref().and_then(|s| state.workspace_of_surface(s));
            ok(
                id,
                json!({"workspace": workspace, "pane": pane, "surface": surface}),
            )
        }

        methods::TREE => {
            let tree = inner.tree().await;
            match serde_json::to_value(tree) {
                Ok(v) => ok(id, v),
                Err(e) => err(id, RpcErrorCode::Internal, format!("tree: {e}")),
            }
        }

        // --- folders -------------------------------------------------------
        methods::FOLDER_NEW => {
            let name = v_str(params, "name").unwrap_or_else(|| "folder".to_string());
            let parent = s(params, "parent");
            let folder = {
                let mut state = inner.state.lock().await;
                if let Some(p) = &parent {
                    if state.folder(p).is_none() {
                        return err(id, RpcErrorCode::NotFound, format!("folder {p} not found"));
                    }
                }
                state.add_folder(name, parent).id
            };
            inner.tree_changed();
            inner.mark_dirty();
            // Every create answers with the new ref, as the mock has always done.
            ok(id, json!({"folder": folder}))
        }
        methods::FOLDER_RENAME => {
            let (folder, name) = (s(params, "folder"), v_str(params, "name"));
            let Some(folder) = folder else {
                return err(id, RpcErrorCode::InvalidParams, "folder required");
            };
            let Some(name) = name else {
                return err(id, RpcErrorCode::InvalidParams, "name required");
            };
            let found = {
                let mut state = inner.state.lock().await;
                match state.folders.iter_mut().find(|f| f.id == folder) {
                    Some(f) => {
                        f.name = name;
                        true
                    }
                    None => false,
                }
            };
            if !found {
                return err(id, RpcErrorCode::NotFound, "folder not found");
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"folder": folder}))
        }
        methods::FOLDER_CLOSE => {
            let Some(folder) = s(params, "folder") else {
                return err(id, RpcErrorCode::InvalidParams, "folder required");
            };
            let closed = {
                let mut state = inner.state.lock().await;
                state.close_folder(&folder)
            };
            if !closed {
                return err(id, RpcErrorCode::NotFound, "folder not found");
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({}))
        }
        methods::FOLDER_MOVE => {
            let Some(folder) = s(params, "folder") else {
                return err(id, RpcErrorCode::InvalidParams, "folder required");
            };
            // An explicit `parent` empties to the top level; an absent one means
            // the same, so a drag onto the empty sidebar area is one shape.
            let parent = match params.get("parent") {
                Some(Value::String(p)) => Some(p.clone()),
                _ => None,
            };
            let index = u(params, "index").map(|n| n as usize);
            let moved = {
                let mut state = inner.state.lock().await;
                state.move_folder(&folder, parent, index)
            };
            match moved {
                Ok(()) => {}
                Err(MoveFolderError::NotFound) => {
                    return err(id, RpcErrorCode::NotFound, format!("folder {folder} not found"));
                }
                Err(MoveFolderError::ParentNotFound) => {
                    return err(id, RpcErrorCode::NotFound, "target parent folder not found");
                }
                Err(MoveFolderError::Cycle) => {
                    return err(
                        id,
                        RpcErrorCode::InvalidParams,
                        "a folder cannot move into itself or one of its own folders",
                    );
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"folder": folder}))
        }

        // --- workspaces ----------------------------------------------------
        methods::WORKSPACE_NEW => {
            let command = s(params, "command");
            let folder = s(params, "folder");
            let cols: u16 = u(params, "cols").unwrap_or(120) as u16;
            let rows: u16 = u(params, "rows").unwrap_or(30) as u16;

            let (ws_ref, pane_ref, surface_ref, title, cwd) = {
                let mut state = inner.state.lock().await;
                if let Some(f) = &folder {
                    if state.folder(f).is_none() {
                        return err(id, RpcErrorCode::NotFound, format!("folder {f} not found"));
                    }
                }
                let title = v_str(params, "name")
                    .or_else(|| {
                        command
                            .as_deref()
                            .and_then(|c| c.split_whitespace().next().map(|s| s.to_string()))
                    })
                    .unwrap_or_else(|| format!("workspace {}", state.counters.workspace + 1));
                let cwd = default_cwd(params, &state, None);
                let ws_ref = state.next_workspace();
                let pane_ref = state.next_pane();
                let surface_ref = state.next_surface();
                let surface = terminal_surface(surface_ref.clone(), title.clone(), cols, rows);
                let pane = Pane {
                    id: pane_ref.clone(),
                    surfaces: vec![surface],
                    active_surface: Some(surface_ref.clone()),
                };
                let ws = Workspace {
                    id: ws_ref.clone(),
                    title: title.clone(),
                    folder,
                    cwd: cwd.clone(),
                    git_branch: None,
                    agent: AgentState::None,
                    summary: None,
                    unread_notifications: 0,
                    status_pills: vec![],
                    progress: None,
                    layout: Layout::Pane {
                        pane: pane_ref.clone(),
                    },
                    panes: vec![pane],
                    focused_pane: Some(pane_ref.clone()),
                    ports: vec![],
                    pr: None,
                };
                state.add_workspace(ws);
                if state.selected_workspace.is_none() {
                    state.selected_workspace = Some(ws_ref.clone());
                }
                (ws_ref, pane_ref, surface_ref, title, cwd)
            };

            if let Err(e) =
                spawn_terminal(inner, &ws_ref, &surface_ref, &title, &cwd, command.as_deref(), cols, rows).await
            {
                let mut state = inner.state.lock().await;
                state.remove_surface(&surface_ref);
                return err(id, RpcErrorCode::Internal, format!("tmux: {e}"));
            }

            // New workspace: detect its branch now so the sidebar has it promptly
            // (the background timer refreshes the rest periodically).
            inner.refresh_branch_of(&ws_ref).await;
            inner.tree_changed();
            inner.mark_dirty();
            ok(
                id,
                json!({"workspace": ws_ref, "pane": pane_ref, "surface": surface_ref}),
            )
        }
        methods::WORKSPACE_IMPORT => import_workspace(inner, id, params).await,
        methods::WORKSPACE_LIST => {
            let state = inner.state.lock().await;
            ok(id, json!({"workspaces": state.workspaces}))
        }
        methods::WORKSPACE_SELECT => {
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            {
                // Looking at a workspace reads its notifications, as in cmux.
                let mut state = inner.state.lock().await;
                state.selected_workspace = Some(ws.clone());
                for n in state.notifications.iter_mut() {
                    if n.workspace.as_deref() == Some(ws.as_str()) {
                        n.read = true;
                    }
                }
                if let Some(w) = state.workspace_mut(&ws) {
                    w.unread_notifications = 0;
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws}))
        }
        methods::WORKSPACE_RENAME => {
            let title = v_str(params, "title");
            let Some(title) = title else {
                return err(id, RpcErrorCode::InvalidParams, "title required");
            };
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            inner.state.lock().await.workspace_mut(&ws).unwrap().title = title;
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws}))
        }
        methods::WORKSPACE_CLOSE => {
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            if params.get("if_idle").and_then(|v| v.as_bool()).unwrap_or(false) {
                let surfaces: Vec<String> = {
                    let state = inner.state.lock().await;
                    state
                        .workspace(&ws)
                        .map(|w| w.panes.iter().flat_map(|p| p.surfaces.iter().map(|s| s.id.clone())).collect())
                        .unwrap_or_default()
                };
                if let Some(cmd) = busy_program(inner, &surfaces).await {
                    return err(id, RpcErrorCode::Busy, &cmd);
                }
            }
            close_workspace(inner, &ws).await;
            ok(id, json!({"workspace": ws}))
        }
        methods::WORKSPACE_MOVE => {
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            let folder = match params.get("folder") {
                Some(Value::Null) | None => None,
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            let index = u(params, "index").map(|n| n as usize);
            let anchor = s(params, "before")
                .map(|a| (a, 0))
                .or_else(|| s(params, "after").map(|a| (a, 1)));
            {
                let mut state = inner.state.lock().await;
                match anchor {
                    // Next to another workspace, joining its folder.
                    Some((a, after)) => {
                        let (Some(cur), Some(at)) = (state.workspace_index(&ws), state.workspace_index(&a)) else {
                            return err(id, RpcErrorCode::NotFound, format!("workspace {a} not found"));
                        };
                        if a != ws {
                            let folder = state.workspaces[at].folder.clone();
                            let at = if cur < at { at - 1 } else { at } + after;
                            state.move_workspace(&ws, folder, Some(at));
                        }
                    }
                    None => {
                        state.move_workspace(&ws, folder, index);
                    }
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws}))
        }
        methods::WORKSPACE_CURRENT => {
            let state = inner.state.lock().await;
            match resolve_workspace(&state, params) {
                Some(ws) => ok(id, json!({"workspace": ws})),
                None => err(id, RpcErrorCode::NotFound, "no workspace"),
            }
        }

        // --- panes / surfaces ---------------------------------------------
        methods::SPLIT_NEW => {
            let side = parse_side(params);
            let kind = s(params, "type").unwrap_or_else(|| "terminal".to_string());
            // The new pane joins the workspace of the pane it splits off, which
            // an explicit surface or pane decides.
            let (ws, target_pane) = {
                let state = inner.state.lock().await;
                let pane = resolve_pane(&state, params);
                let ws = pane
                    .as_ref()
                    .and_then(|p| state.locate_pane(p).map(|(wi, _)| state.workspaces[wi].id.clone()));
                (ws, pane)
            };
            let (Some(ws), Some(target_pane)) = (ws, target_pane) else {
                return err(id, RpcErrorCode::NotFound, "no target pane");
            };
            split_impl(inner, id, &ws, &target_pane, side, &kind, params).await
        }
        methods::PANE_LIST => {
            let state = inner.state.lock().await;
            let ws = resolve_workspace(&state, params);
            let Some(ws) = ws else {
                return ok(id, json!({"panes": []}));
            };
            let panes = state.workspace(&ws).map(|w| w.panes.clone()).unwrap_or_default();
            let focused = state.workspace(&ws).and_then(|w| w.focused_pane.clone());
            ok(id, json!({"workspace": ws, "focused_pane": focused, "panes": panes}))
        }
        methods::PANE_SURFACES => {
            let state = inner.state.lock().await;
            let Some(pane) = resolve_pane(&state, params) else {
                return err(id, RpcErrorCode::NotFound, "pane not found");
            };
            let Some((wi, pi)) = state.locate_pane(&pane) else {
                return err(id, RpcErrorCode::NotFound, "pane not found");
            };
            let p = &state.workspaces[wi].panes[pi];
            let surfaces: Vec<Value> = p
                .surfaces
                .iter()
                .map(|x| {
                    let selected = p.active_surface.as_deref() == Some(x.id.as_str());
                    match &x.kind {
                        SurfaceKind::Terminal { .. } => json!({
                            "id": x.id, "title": x.title, "type": "terminal", "selected": selected,
                        }),
                        SurfaceKind::Browser { url, .. } => json!({
                            "id": x.id, "title": x.title, "type": "browser", "selected": selected, "url": url,
                        }),
                    }
                })
                .collect();
            ok(
                id,
                json!({"workspace": state.workspaces[wi].id, "pane": pane, "surfaces": surfaces}),
            )
        }
        methods::PANE_FOCUS => {
            let Some(pane) = s(params, "pane") else {
                return err(id, RpcErrorCode::InvalidParams, "pane required");
            };
            let ws = {
                let mut state = inner.state.lock().await;
                match state.locate_pane(&pane) {
                    Some((wi, pi)) => {
                        let ws_id = state.workspaces[wi].id.clone();
                        let fallback = state.workspaces[wi].panes[pi]
                            .surfaces
                            .first()
                            .map(|s| s.id.clone());
                        let p = &mut state.workspaces[wi].panes[pi];
                        if p.active_surface.is_none() {
                            p.active_surface = fallback;
                        }
                        state.workspaces[wi].focused_pane = Some(pane.clone());
                        Some(ws_id)
                    }
                    None => None,
                }
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "pane not found");
            };
            inner.tree_changed();
            ok(id, json!({"workspace": ws, "pane": pane}))
        }
        methods::PANE_RESIZE => {
            let ratio = f(params, "ratio").unwrap_or(0.5);
            let resized = {
                let mut state = inner.state.lock().await;
                // The pane's own workspace, not the caller's or the selected
                // one: a pane named from elsewhere would otherwise be missed.
                resolve_pane(&state, params).and_then(|pane| {
                    let (wi, _) = state.locate_pane(&pane)?;
                    let ws = state.workspaces[wi].id.clone();
                    state
                        .resize_pane(&ws, &pane, ratio)
                        .then_some((ws, pane))
                })
            };
            let Some((ws, pane)) = resized else {
                return err(id, RpcErrorCode::NotFound, "pane not found in a split");
            };
            inner.tree_changed();
            ok(id, json!({"workspace": ws, "pane": pane, "ratio": ratio}))
        }
        methods::SURFACE_NEW => {
            let kind = s(params, "type").unwrap_or_else(|| "terminal".to_string());
            let pane = {
                let state = inner.state.lock().await;
                resolve_pane(&state, params)
            };
            let Some(pane) = pane else {
                return err(id, RpcErrorCode::NotFound, "pane not found");
            };
            add_surface_impl(inner, id, &pane, &kind, params).await
        }
        methods::SURFACE_CLOSE => {
            let surface = {
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            if params.get("if_idle").and_then(|v| v.as_bool()).unwrap_or(false) {
                if let Some(cmd) = busy_program(inner, std::slice::from_ref(&surface)).await {
                    return err(id, RpcErrorCode::Busy, &cmd);
                }
            }
            close_surface(inner, &surface).await;
            ok(id, json!({"surface": surface}))
        }
        methods::SURFACE_RENAME => {
            let Some(title) = v_str(params, "title") else {
                return err(id, RpcErrorCode::InvalidParams, "title required");
            };
            let surface = {
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            {
                let mut state = inner.state.lock().await;
                if let Some(s) = state.surface_mut(&surface) {
                    s.title = title;
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"surface": surface}))
        }

        // --- drag-and-drop rearranging ------------------------------------
        methods::SURFACE_MOVE => {
            let surface = {
                // `workspace` names the destination here, not where the tab is.
                let mut own = params.clone();
                if let Some(o) = own.as_object_mut() {
                    o.remove("workspace");
                }
                let state = inner.state.lock().await;
                resolve_surface(&state, &own)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            let res = {
                let mut state = inner.state.lock().await;
                let target = match move_target(&state, &surface, params) {
                    Ok(t) => t,
                    Err(e) => return RpcResponse { id, result: None, error: Some(e) },
                };
                let from = state.workspace_of_surface(&surface);
                state
                    .transfer_surface(&surface, &target.0, target.1)
                    .map(|(pane, _)| (pane, from, state.workspace_of_surface(&surface)))
            };
            match res {
                Ok((pane, from, to)) => {
                    if from != to {
                        retag_workspace(inner, &surface).await;
                    }
                    inner.tree_changed();
                    inner.mark_dirty();
                    ok(id, json!({"pane": pane, "surface": surface, "workspace": to}))
                }
                Err(e) => RpcResponse {
                    id,
                    result: None,
                    error: Some(move_err(e)),
                },
            }
        }
        methods::SURFACE_MOVE_TO_NEW_WORKSPACE => {
            let res = {
                let mut state = inner.state.lock().await;
                let Some(surface) = resolve_surface(&state, params) else {
                    return err(id, RpcErrorCode::NotFound, "surface not found");
                };
                state
                    .surface_to_new_workspace(&surface, v_str(params, "title"))
                    .map(|(ws, pane)| {
                        if b(params, "focus").unwrap_or(false) {
                            state.selected_workspace = Some(ws.clone());
                        }
                        (ws, pane, surface)
                    })
            };
            match res {
                Ok((ws, pane, surface)) => {
                    retag_workspace(inner, &surface).await;
                    inner.refresh_branch_of(&ws).await;
                    inner.tree_changed();
                    inner.mark_dirty();
                    ok(id, json!({"workspace": ws, "pane": pane, "surface": surface}))
                }
                Err(e) => RpcResponse {
                    id,
                    result: None,
                    error: Some(move_err(e)),
                },
            }
        }
        methods::SURFACE_SPLIT => {
            let surface = {
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            let side = parse_side(params);
            // `pane` names the pane the split should come off; without it the
            // surface's own pane is split (the CLI's split-here form).
            let target = s(params, "pane");
            let res = {
                let mut state = inner.state.lock().await;
                match target {
                    Some(pane) => state.split_surface_into(&surface, &pane, side),
                    None => state.split_surface(&surface, side),
                }
            };
            match res {
                Ok(pane) => {
                    inner.tree_changed();
                    inner.mark_dirty();
                    ok(id, json!({"pane": pane, "surface": surface}))
                }
                Err(e) => RpcResponse {
                    id,
                    result: None,
                    error: Some(move_err(e)),
                },
            }
        }
        methods::PANE_SWAP => {
            let (Some(pane), Some(other)) = (s(params, "pane"), s(params, "other")) else {
                return err(id, RpcErrorCode::InvalidParams, "pane and other required");
            };
            let ws = {
                let state = inner.state.lock().await;
                match (
                    state.locate_pane(&pane),
                    state.locate_pane(&other),
                ) {
                    (Some((wi, _)), Some((wi2, _))) if wi == wi2 => {
                        Some(state.workspaces[wi].id.clone())
                    }
                    _ => None,
                }
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "panes not found in one workspace");
            };
            let ok_swap = {
                let mut state = inner.state.lock().await;
                state.swap_panes(&ws, &pane, &other)
            };
            if !ok_swap {
                return err(id, RpcErrorCode::NotFound, "panes not found in one workspace");
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws, "pane": pane, "other": other}))
        }

        methods::TERM_SEND => {
            let surface = {
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            let Some(text) = s(params, "text") else {
                return err(id, RpcErrorCode::InvalidParams, "text required");
            };
            let term = inner.surface_term(&surface).await;
            let Some(term) = term else {
                return err(id, RpcErrorCode::InvalidParams, "not a terminal surface");
            };
            // `\n` acts as Enter.
            let bytes: Vec<u8> = text.bytes().map(|b| if b == b'\n' { b'\r' } else { b }).collect();
            if let Err(e) = inner.tmux.send_bytes(&term.pane, &bytes).await {
                return err(id, RpcErrorCode::Internal, format!("tmux: {e}"));
            }
            ok(id, json!({"surface": surface}))
        }
        methods::TERM_SEND_KEY => {
            let surface = {
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            let Some(key) = s(params, "key") else {
                return err(id, RpcErrorCode::InvalidParams, "key required");
            };
            let term = inner.surface_term(&surface).await;
            let Some(term) = term else {
                return err(id, RpcErrorCode::InvalidParams, "not a terminal surface");
            };
            if let Err(e) = inner.tmux.send_named_key(&term.pane, &key).await {
                return err(id, RpcErrorCode::InvalidParams, format!("{e}"));
            }
            ok(id, json!({"surface": surface}))
        }
        methods::TERM_READ_SCREEN => {
            let surface = {
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            let term = inner.surface_term(&surface).await;
            let Some(term) = term else {
                return err(id, RpcErrorCode::InvalidParams, "not a terminal surface");
            };
            let scrollback = b(params, "scrollback").unwrap_or(false);
            let lines = u(params, "lines").map(|n| n.min(u32::MAX as u64) as u32);
            // tmux capture-pane's -S/-E take a line number or "-".
            let line_arg = |key: &str| match params.get(key) {
                Some(Value::String(v)) => Some(v.clone()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            let (start, end) = (line_arg("start"), line_arg("end"));
            let escape = b(params, "escape").unwrap_or(false);
            let join = b(params, "join").unwrap_or(false);
            let res = if start.is_some() || end.is_some() || escape || join {
                for v in [&start, &end].into_iter().flatten() {
                    if !crate::tmux::valid_capture_line(v) {
                        return err(id, RpcErrorCode::InvalidParams, format!("bad line {v:?}: use a number or '-'"));
                    }
                }
                inner
                    .tmux
                    .capture_range(&term.pane, start.as_deref(), end.as_deref(), escape, join)
                    .await
            } else {
                inner.tmux.read_screen(&term.pane, scrollback, lines).await
            };
            match res {
                Ok(text) => ok(id, json!({"text": text, "surface": surface})),
                Err(e) => err(id, RpcErrorCode::Internal, format!("tmux: {e}")),
            }
        }
        methods::TERM_CLEAR_HISTORY => {
            let surface = {
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            if let Some(term) = inner.surface_term(&surface).await {
                let _ = inner.tmux.clear_history(&term.pane).await;
            }
            ok(id, json!({"surface": surface}))
        }
        methods::TERM_PROCESSES => {
            let wins = match inner.tmux.list_windows().await {
                Ok(w) => w,
                Err(e) => return err(id, RpcErrorCode::Internal, format!("tmux: {e}")),
            };
            let pids = inner.tmux.pane_pids().await.unwrap_or_default();
            let state = inner.state.lock().await;
            let terminals: Vec<Value> = wins
                .iter()
                .filter_map(|w| {
                    let surface = state.surface_of_pane(&w.pane)?;
                    let agent = state
                        .resume
                        .get(&surface)
                        .and_then(|r| r.agent.as_ref())
                        .map(|a| format!("{}:{}", a.agent, a.id))
                        .or_else(|| w.agent_session.clone());
                    Some(json!({
                        "surface": surface,
                        "pid": pids.get(&w.pane),
                        "command": w.command,
                        "cwd": w.cwd,
                        "agent_session": agent,
                    }))
                })
                .collect();
            ok(id, json!({"terminals": terminals}))
        }

        // --- notifications / status ---------------------------------------
        methods::NOTIFY => {
            let title = v_str(params, "title").unwrap_or_else(|| "lmux".to_string());
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let notification = {
                let mut state = inner.state.lock().await;
                state.add_notification(Notification {
                    id: String::new(),
                    title,
                    subtitle: v_str(params, "subtitle"),
                    body: v_str(params, "body"),
                    workspace: ws,
                    surface: s(params, "surface"),
                    created_at: now_ms(),
                    read: false,
                })
            };
            inner
                .broadcast(Out::Msg(ServerMsg::Notification {
                    notification: notification.clone(),
                }))
                .await;
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"id": notification.id}))
        }
        methods::NOTIFY_LIST => {
            let state = inner.state.lock().await;
            ok(id, json!({"notifications": state.notifications}))
        }
        methods::NOTIFY_CLEAR => {
            // Idempotent: `count` says how many went, so a caller that named
            // one can tell a stale id from a dismissed one.
            let count = {
                let mut state = inner.state.lock().await;
                let sel = NotifySelector::from_params(&state, params, "all_read");
                let before = state.notifications.len();
                state.notifications.retain(|n| !sel.matches(n));
                recount_unread(&mut state);
                before - state.notifications.len()
            };
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"count": count}))
        }
        methods::FILE_PUT => {
            // A file from the app (a pasted image) saved where programs on
            // this host can read it; the app then pastes the path.
            use base64::Engine;
            let name = s(params, "name").unwrap_or_else(|| "paste.png".into());
            let safe: String = name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '-' })
                .collect();
            let Some(data) = params.get("data").and_then(|d| d.as_str()) else {
                return err(id, RpcErrorCode::InvalidParams, "data required");
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
                return err(id, RpcErrorCode::InvalidParams, "data is not base64");
            };
            let dir = inner.cfg.state_dir.join("pastes");
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let path = dir.join(format!("{stamp}-{safe}"));
            let written = async {
                tokio::fs::create_dir_all(&dir).await?;
                tokio::fs::write(&path, &bytes).await
            }
            .await;
            match written {
                Ok(()) => ok(id, json!({"path": path.to_string_lossy()})),
                Err(e) => err(id, RpcErrorCode::Internal, format!("saving {}: {e}", path.display())),
            }
        }
        methods::NOTIFY_MARK_READ => {
            let count = {
                let mut state = inner.state.lock().await;
                let sel = NotifySelector::from_params(&state, params, "all");
                let mut count = 0;
                for n in state.notifications.iter_mut().filter(|n| sel.matches(n)) {
                    n.read = true;
                    count += 1;
                }
                recount_unread(&mut state);
                count
            };
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"count": count}))
        }
        methods::NOTIFY_JUMP_TO_UNREAD => {
            let picked = {
                let state = inner.state.lock().await;
                match s(params, "id") {
                    Some(nid) => match state.notifications.iter().find(|n| n.id == nid) {
                        Some(n) => Some(n.clone()),
                        None => {
                            return err(id, RpcErrorCode::NotFound, format!("notification {nid} not found"))
                        }
                    },
                    // The newest unread one whose workspace still exists.
                    None => state
                        .notifications
                        .iter()
                        .filter(|n| !n.read)
                        .filter(|n| n.workspace.as_deref().is_some_and(|w| state.workspace(w).is_some()))
                        .max_by_key(|n| n.created_at)
                        .cloned(),
                }
            };
            let Some(n) = picked else {
                return ok(id, json!({}));
            };
            {
                let mut state = inner.state.lock().await;
                if let Some(ws) = n.workspace.as_deref().filter(|w| state.workspace(w).is_some()) {
                    // Looking at a workspace reads its notifications, as select does.
                    let ws = ws.to_string();
                    state.selected_workspace = Some(ws.clone());
                    for x in state.notifications.iter_mut() {
                        if x.workspace.as_deref() == Some(ws.as_str()) {
                            x.read = true;
                        }
                    }
                }
                if let Some(x) = state.notifications.iter_mut().find(|x| x.id == n.id) {
                    x.read = true;
                }
                if let Some(surf) = &n.surface {
                    state.set_active_surface(surf);
                }
                recount_unread(&mut state);
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"id": n.id, "workspace": n.workspace, "surface": n.surface}))
        }
        methods::STATUS_SET => {
            let Some(key) = v_str(params, "key") else {
                return err(id, RpcErrorCode::InvalidParams, "key required");
            };
            let value = v_str(params, "value").unwrap_or_default();
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            let pill = StatusPill {
                key: key.clone(),
                value,
                icon: v_str(params, "icon"),
                color: v_str(params, "color"),
            };
            {
                let mut state = inner.state.lock().await;
                if let Some(w) = state.workspace_mut(&ws) {
                    if let Some(existing) = w.status_pills.iter_mut().find(|p| p.key == key) {
                        *existing = pill;
                    } else {
                        w.status_pills.push(pill);
                    }
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws, "key": key}))
        }
        methods::STATUS_CLEAR => {
            let Some(key) = v_str(params, "key") else {
                return err(id, RpcErrorCode::InvalidParams, "key required");
            };
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            {
                let mut state = inner.state.lock().await;
                if let Some(w) = state.workspace_mut(&ws) {
                    w.status_pills.retain(|p| p.key != key);
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws, "key": key}))
        }
        methods::PROGRESS_SET => {
            let value = f(params, "value").unwrap_or(0.0).clamp(0.0, 1.0);
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            {
                let mut state = inner.state.lock().await;
                if let Some(w) = state.workspace_mut(&ws) {
                    w.progress = Some(Progress {
                        value,
                        label: v_str(params, "label"),
                    });
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws}))
        }
        methods::PROGRESS_CLEAR => {
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            {
                let mut state = inner.state.lock().await;
                if let Some(w) = state.workspace_mut(&ws) {
                    w.progress = None;
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws}))
        }
        methods::LOG => {
            let level = match s(params, "level").as_deref() {
                Some("success") => LogLevel::Success,
                Some("warning") | Some("warn") => LogLevel::Warning,
                Some("error") => LogLevel::Error,
                _ => LogLevel::Info,
            };
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let entry = LogEntry {
                level,
                source: v_str(params, "source"),
                message: v_str(params, "message").unwrap_or_default(),
                created_at: now_ms(),
            };
            {
                let mut state = inner.state.lock().await;
                state.push_log(entry.clone());
            }
            inner
                .broadcast(Out::Msg(ServerMsg::Log {
                    workspace: ws,
                    entry,
                }))
                .await;
            inner.mark_dirty();
            ok(id, json!({}))
        }
        methods::LOG_LIST => {
            // LogEntry carries no workspace field in the proto; filters by
            // workspace are therefore ignored here (see proto change requests).
            let limit = u(params, "limit").unwrap_or(100) as usize;
            let state = inner.state.lock().await;
            let all: Vec<LogEntry> = state.logs.clone();
            let from = all.len().saturating_sub(limit);
            ok(id, json!({"entries": &all[from..]}))
        }
        methods::LOG_CLEAR => {
            inner.state.lock().await.logs.clear();
            inner.mark_dirty();
            ok(id, json!({}))
        }
        methods::STATUS_LIST => {
            let state = inner.state.lock().await;
            let Some(w) = resolve_workspace(&state, params).and_then(|ws| state.workspace(&ws)) else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            ok(
                id,
                json!({"workspace": w.id, "status": w.status_pills, "progress": w.progress}),
            )
        }
        methods::AGENT_SESSION => {
            let Some(surface) = s(params, "surface") else {
                return err(id, RpcErrorCode::InvalidParams, "surface required");
            };
            let agent = s(params, "agent").unwrap_or_else(|| "claude".to_string());
            let session = s(params, "id").unwrap_or_default();
            if agent != "claude" {
                return err(id, RpcErrorCode::InvalidParams, "only agent \"claude\" can be resumed");
            }
            if !crate::tree::valid_agent_session_id(&session) {
                return err(id, RpcErrorCode::InvalidParams, "invalid session id");
            }
            let window = {
                let mut state = inner.state.lock().await;
                if state.surface(&surface).is_none() {
                    return err(id, RpcErrorCode::NotFound, "surface not found");
                }
                let r = state.resume_mut(&surface);
                r.agent = Some(crate::tree::AgentSession {
                    agent: agent.clone(),
                    id: session.clone(),
                });
                if let Some(cwd) = s(params, "cwd").filter(|c| !c.is_empty()) {
                    r.cwd = Some(cwd);
                }
                state.term_cloned(&surface).map(|t| t.window)
            };
            if let Some(window) = window {
                let _ = inner
                    .tmux
                    .set_window_option(&window, "@lmux_agent_session", &format!("{agent}:{session}"))
                    .await;
            }
            inner.mark_dirty();
            ok(id, json!({}))
        }
        methods::AGENT_STATE => {
            let Some(state_str) = s(params, "state") else {
                return err(id, RpcErrorCode::InvalidParams, "state required");
            };
            let agent: AgentState = match state_str.as_str() {
                "running" => AgentState::Running,
                "idle" => AgentState::Idle,
                "needs_input" => AgentState::NeedsInput,
                _ => AgentState::None,
            };
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            {
                let mut state = inner.state.lock().await;
                if let Some(w) = state.workspace_mut(&ws) {
                    w.agent = agent;
                    if let Some(s) = v_str(params, "summary") {
                        w.summary = Some(s);
                    }
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws}))
        }

        // --- browser -------------------------------------------------------
        methods::BROWSER_OPEN => {
            if inner.browser.is_none() && !inner.cfg.app_browser {
                return err(id, RpcErrorCode::Unsupported, unsupported(&req.method).message);
            }
            let side = parse_side(params);
            let (ws, target_pane, none_at_all) = {
                let state = inner.state.lock().await;
                // The pane to split off belongs to the workspace it is in, which
                // an explicit surface decides.
                let pane = resolve_pane(&state, params);
                let ws = pane
                    .as_ref()
                    .and_then(|p| state.locate_pane(p).map(|(wi, _)| state.workspaces[wi].id.clone()))
                    .or_else(|| resolve_workspace(&state, params));
                (ws, pane, state.workspaces.is_empty())
            };
            if none_at_all && s(params, "workspace").is_none() && s(params, "surface").is_none() {
                return browser_workspace(inner, id, params).await;
            }
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "workspace not found");
            };
            let Some(target_pane) = target_pane else {
                return err(id, RpcErrorCode::NotFound, "no target pane");
            };
            split_impl(inner, id, &ws, &target_pane, side, "browser", params).await
        }
        methods::BROWSER_REPORT => {
            let Some(surface) = s(params, "surface") else {
                return err(id, RpcErrorCode::InvalidParams, "surface required");
            };
            {
                let mut state = inner.state.lock().await;
                let Some(sf) = state.surface_mut(&surface) else {
                    return err(id, RpcErrorCode::NotFound, "surface not found");
                };
                let SurfaceKind::Browser { url, loading, can_go_back, can_go_forward, .. } = &mut sf.kind else {
                    return err(id, RpcErrorCode::InvalidParams, "not a browser surface");
                };
                if let Some(v) = s(params, "url") {
                    *url = v;
                }
                if let Some(v) = b(params, "loading") {
                    *loading = v;
                }
                if let Some(v) = b(params, "can_go_back") {
                    *can_go_back = v;
                }
                if let Some(v) = b(params, "can_go_forward") {
                    *can_go_forward = v;
                }
                if let Some(v) = v_str(params, "title").filter(|t| !t.trim().is_empty()) {
                    sf.title = v;
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"surface": surface}))
        }
        methods::BROWSER_MOUSE | methods::BROWSER_KEYBOARD => {
            browser_input_rpc(inner, id, &req.method, params).await
        }
        m if m.starts_with("browser.") => {
            // A browser the app draws: the app runs the command's steps.
            let in_app = {
                let state = inner.state.lock().await;
                resolve_browser_surface(&state, params).filter(|sf| {
                    matches!(state.surface(sf).map(|x| &x.kind), Some(SurfaceKind::Browser { in_app: true, .. }))
                })
            };
            if let Some(surface) = in_app {
                return match crate::app_browser::handle_rpc(&crate::app_browser::ViaApp(inner), &surface, m, params).await {
                    Ok(v) => ok(id, v),
                    Err(e) => RpcResponse { id, result: None, error: Some(e) },
                };
            }
            let Some(backend) = &inner.browser else {
                return err(id, RpcErrorCode::Unsupported, unsupported(m).message);
            };
            let surface = {
                let state = inner.state.lock().await;
                resolve_browser_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            match backend.handle_rpc(&surface, m, params.clone()).await {
                Ok(v) => ok(id, v),
                Err(e) => RpcResponse {
                    id,
                    result: None,
                    error: Some(e),
                },
            }
        }

        other => err(
            id,
            RpcErrorCode::UnknownMethod,
            format!("unknown method: {other}"),
        ),
    }
}

/// `browser.mouse` and `browser.keyboard`: real pointer and key events, sent
/// the way the app sends a viewer's input. They go through the engine's own
/// input path, so they land in cross-origin iframes (Google's sign-in button,
/// payment frames) that element refs and DOM events cannot reach.
async fn browser_input_rpc(inner: &Arc<Inner>, id: u64, method: &str, params: &Value) -> RpcResponse {
    let (surface, in_app) = {
        let state = inner.state.lock().await;
        let surface = resolve_browser_surface(&state, params);
        let in_app = surface
            .as_ref()
            .is_some_and(|sf| matches!(state.surface(sf).map(|x| &x.kind), Some(SurfaceKind::Browser { in_app: true, .. })));
        (surface, in_app)
    };
    let Some(surface) = surface else {
        return err(id, RpcErrorCode::NotFound, "surface not found");
    };
    if in_app {
        return err(
            id,
            RpcErrorCode::Unsupported,
            "real pointer and key input is not available for browsers the lmux app draws itself; \
             use a browser on a host (a WebKit helper or Chromium)",
        );
    }
    let Some(backend) = &inner.browser else {
        return err(id, RpcErrorCode::Unsupported, unsupported(method).message);
    };
    let events = match input_events(method, params) {
        Ok(e) => e,
        Err(m) => return err(id, RpcErrorCode::InvalidParams, m),
    };
    let n = events.len();
    for ev in events {
        if let Err(e) = backend.input(&surface, ev).await {
            return err(id, RpcErrorCode::Internal, format!("input: {e}"));
        }
        // Pages see hover, press and release as separate moments.
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    ok(id, json!({ "events": n }))
}

fn input_mods(params: &Value) -> lmux_proto::Modifiers {
    let flag = |k: &str| {
        params
            .get("modifiers")
            .and_then(|m| m.get(k))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    lmux_proto::Modifiers { alt: flag("alt"), ctrl: flag("ctrl"), meta: flag("meta"), shift: flag("shift") }
}

/// DOM `KeyboardEvent.code` for a `key` as typed.
fn dom_code(key: &str) -> String {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => format!("Key{}", c.to_ascii_uppercase()),
        (Some(c), None) if c.is_ascii_digit() => format!("Digit{c}"),
        (Some(' '), None) => "Space".into(),
        _ => key.to_owned(),
    }
}

/// The input events for a `browser.mouse` or `browser.keyboard` request.
pub(crate) fn input_events(method: &str, params: &Value) -> Result<Vec<lmux_proto::BrowserInputEvent>, String> {
    use lmux_proto::{BrowserInputEvent as E, KeyAction, MouseAction, MouseButton};
    let mods = input_mods(params);
    if method == methods::BROWSER_MOUSE {
        let num = |k: &str| params.get(k).and_then(Value::as_f64);
        let action = s(params, "action").unwrap_or_else(|| "click".into());
        let (Some(x), Some(y)) = (num("x"), num("y")) else {
            return Err("x and y are required (CSS pixels from the top left of the page)".into());
        };
        let button = match s(params, "button").as_deref() {
            None | Some("left") => MouseButton::Left,
            Some("right") => MouseButton::Right,
            Some("middle") => MouseButton::Middle,
            Some(other) => return Err(format!("unknown button `{other}` (left, right, middle)")),
        };
        let mouse = |action: MouseAction, button: MouseButton, click_count: u32| E::Mouse {
            action,
            x,
            y,
            button,
            click_count,
            modifiers: mods,
        };
        return match action.as_str() {
            "move" => Ok(vec![mouse(MouseAction::Move, MouseButton::None, 0)]),
            "down" => Ok(vec![mouse(MouseAction::Move, MouseButton::None, 0), mouse(MouseAction::Down, button, 1)]),
            "up" => Ok(vec![mouse(MouseAction::Up, button, 1)]),
            "click" | "dblclick" => {
                let n = if action == "dblclick" {
                    2
                } else {
                    params.get("count").and_then(Value::as_u64).unwrap_or(1).clamp(1, 3) as u32
                };
                let mut out = vec![mouse(MouseAction::Move, MouseButton::None, 0)];
                for i in 1..=n {
                    out.push(mouse(MouseAction::Down, button, i));
                    out.push(mouse(MouseAction::Up, button, i));
                }
                Ok(out)
            }
            "wheel" => Ok(vec![E::Wheel {
                x,
                y,
                delta_x: num("dx").unwrap_or(0.0),
                delta_y: num("dy").unwrap_or(0.0),
                modifiers: mods,
            }]),
            other => Err(format!("unknown action `{other}` (click, dblclick, down, up, move, wheel)")),
        };
    }

    // browser.keyboard
    let mut out = Vec::new();
    if let Some(text) = s(params, "text").filter(|t| !t.is_empty()) {
        out.push(E::InsertText { text });
    }
    if let Some(combo) = s(params, "key").filter(|k| !k.is_empty()) {
        let (key, ctrl, alt, shift, meta) = crate::app_browser::parse_key(&combo);
        if key.is_empty() {
            return Err(format!("unknown key `{combo}`"));
        }
        let mods = lmux_proto::Modifiers { alt, ctrl, meta, shift };
        // A character key produces its text unless Ctrl or Cmd turn it into a
        // shortcut.
        let text = (key.chars().count() == 1 && !ctrl && !meta).then(|| key.clone());
        let code = dom_code(&key);
        out.push(E::Key { action: KeyAction::Down, key: key.clone(), code: code.clone(), text, modifiers: mods });
        out.push(E::Key { action: KeyAction::Up, key, code, text: None, modifiers: mods });
    }
    if out.is_empty() {
        return Err("give `text` to type, or `key` to press (ctrl+a, Enter, ...)".into());
    }
    Ok(out)
}

/// `browser.open` with no workspace to put it in: a new workspace holding just
/// the browser.
async fn browser_workspace(inner: &Arc<Inner>, id: u64, params: &Value) -> RpcResponse {
    if inner.browser.is_none() && !inner.cfg.app_browser {
        return err(id, RpcErrorCode::Unsupported, "browser backend not available");
    }
    let in_app = browsers_in_app(inner);
    let url = s(params, "url");
    let (ws_ref, pane_ref, surface_ref) = {
        let mut state = inner.state.lock().await;
        let ws_ref = state.next_workspace();
        let pane_ref = state.next_pane();
        let surface_ref = state.next_surface();
        let cwd = default_cwd(params, &state, None);
        state.add_workspace(Workspace {
            id: ws_ref.clone(),
            title: "browser".into(),
            folder: None,
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
                surfaces: vec![browser_surface(surface_ref.clone(), url.as_deref(), in_app)],
                active_surface: Some(surface_ref.clone()),
            }],
            focused_pane: Some(pane_ref.clone()),
            ports: vec![],
            pr: None,
        });
        if state.selected_workspace.is_none() {
            state.selected_workspace = Some(ws_ref.clone());
        }
        (ws_ref, pane_ref, surface_ref)
    };
    // Removing its only surface closes the workspace too.
    if let Err(e) = create_page(inner, &surface_ref, url.as_deref()).await {
        return err(id, RpcErrorCode::Internal, e);
    }
    inner.tree_changed();
    inner.mark_dirty();
    ok(id, json!({"workspace": ws_ref, "pane": pane_ref, "surface": surface_ref}))
}

/// Run another method from inside a handler, as its own request.
async fn sub_call(inner: &Arc<Inner>, method: &str, params: Value) -> Result<Value, String> {
    let req = RpcRequest { id: 0, method: method.to_string(), params };
    let resp = Box::pin(dispatch(inner, &req)).await;
    match (resp.result, resp.error) {
        (_, Some(e)) => Err(format!("{method}: {}", e.message)),
        (r, None) => Ok(r.unwrap_or(Value::Null)),
    }
}

/// `workspace.import`: build a whole workspace from a spec (an imported cmux
/// workspace): its folder, every pane and tab, the split tree with its ratios,
/// which tab is selected, and the agent session each terminal held.
async fn import_workspace(inner: &Arc<Inner>, id: u64, params: &Value) -> RpcResponse {
    use lmux_proto::cmux::ImportWorkspace;
    let spec: ImportWorkspace = match serde_json::from_value(params["workspace"].clone()) {
        Ok(w) => w,
        Err(e) => return err(id, RpcErrorCode::InvalidParams, format!("workspace: {e}")),
    };
    let resume = b(params, "resume").unwrap_or(false);
    match build_import(inner, &spec, resume).await {
        Ok(v) => ok(id, v),
        Err(e) => err(id, RpcErrorCode::Internal, e),
    }
}

async fn build_import(
    inner: &Arc<Inner>,
    spec: &lmux_proto::cmux::ImportWorkspace,
    resume: bool,
) -> Result<Value, String> {
    use lmux_proto::cmux::{ImportLayout, ImportTab};

    fn leaves<'a>(l: &'a ImportLayout, out: &mut Vec<(&'a [ImportTab], usize, bool)>) {
        match l {
            ImportLayout::Pane { tabs, selected, focused } => out.push((tabs, *selected, *focused)),
            ImportLayout::Split { first, second, .. } => {
                leaves(first, out);
                leaves(second, out);
            }
        }
    }
    fn tree(l: &ImportLayout, panes: &mut std::slice::Iter<Ref>) -> Layout {
        match l {
            ImportLayout::Pane { .. } => Layout::Pane { pane: panes.next().cloned().unwrap_or_default() },
            ImportLayout::Split { direction, ratio, first, second } => Layout::Split {
                direction: *direction,
                ratio: ratio.clamp(0.05, 0.95),
                first: Box::new(tree(first, panes)),
                second: Box::new(tree(second, panes)),
            },
        }
    }
    fn tab_params(t: &ImportTab) -> Value {
        let mut p = json!({"type": t.kind, "focus": false});
        if let Some(u) = &t.url {
            p["url"] = json!(u);
        }
        if let Some(c) = &t.cwd {
            p["cwd"] = json!(c);
        }
        if let Some(c) = &t.command {
            p["command"] = json!(c);
        }
        p
    }

    let mut list = vec![];
    leaves(&spec.layout, &mut list);
    if list.is_empty() || list[0].0.is_empty() {
        return Err("workspace has no tabs".into());
    }

    let folder = match &spec.folder {
        Some(name) => {
            let mut state = inner.state.lock().await;
            let found = state.folders.iter().find(|f| &f.name == name && f.parent.is_none()).map(|f| f.id.clone());
            Some(found.unwrap_or_else(|| state.add_folder(name.clone(), None).id))
        }
        None => None,
    };

    // The workspace starts with a terminal; a workspace whose first tab is a
    // browser gets that browser added and the starting terminal closed.
    let first = &list[0].0[0];
    let mut p = json!({"name": spec.title});
    if let Some(f) = &folder {
        p["folder"] = json!(f);
    }
    if first.kind == "terminal" {
        if let Some(c) = &first.cwd {
            p["cwd"] = json!(c);
        }
        if let Some(c) = &first.command {
            p["command"] = json!(c);
        }
    }
    let made = sub_call(inner, methods::WORKSPACE_NEW, p).await?;
    let ws = made["workspace"].as_str().unwrap_or_default().to_string();
    let pane0 = made["pane"].as_str().unwrap_or_default().to_string();
    let start = made["surface"].as_str().unwrap_or_default().to_string();
    let placeholder = (first.kind != "terminal").then(|| start.clone());

    let built: Result<_, String> = async {
        let mut panes: Vec<Ref> = vec![];
        let mut surfaces: Vec<Vec<Ref>> = vec![];
        for (i, (tabs, _, _)) in list.iter().enumerate() {
            let mut refs = vec![];
            let mut rest = tabs.iter();
            let pane = if i == 0 {
                if placeholder.is_none() {
                    refs.push(start.clone());
                    rest.next();
                }
                pane0.clone()
            } else {
                let t = rest.next().unwrap();
                let mut p = tab_params(t);
                p["pane"] = json!(panes.last().unwrap());
                p["side"] = json!("right");
                let r = sub_call(inner, methods::SPLIT_NEW, p).await?;
                refs.push(r["surface"].as_str().unwrap_or_default().to_string());
                r["pane"].as_str().unwrap_or_default().to_string()
            };
            for t in rest {
                let mut p = tab_params(t);
                p["pane"] = json!(pane);
                let r = sub_call(inner, methods::SURFACE_NEW, p).await?;
                refs.push(r["surface"].as_str().unwrap_or_default().to_string());
            }
            panes.push(pane);
            surfaces.push(refs);
        }
        Ok((panes, surfaces))
    }
    .await;
    let (panes, surfaces) = match built {
        Ok(v) => v,
        Err(e) => {
            // Leave nothing half-built behind.
            close_workspace(inner, &ws).await;
            return Err(e);
        }
    };
    if let Some(ph) = &placeholder {
        close_surface(inner, ph).await;
    }

    {
        let mut state = inner.state.lock().await;
        let layout = tree(&spec.layout, &mut panes.iter());
        let focused = list.iter().position(|l| l.2).unwrap_or(0);
        for (i, (_, selected, _)) in list.iter().enumerate() {
            if let Some(s) = surfaces[i].get(*selected).or(surfaces[i].first()) {
                state.set_active_surface(s);
            }
        }
        if let Some(w) = state.workspace_mut(&ws) {
            w.layout = layout;
            w.focused_pane = panes.get(focused).cloned();
        }
    }

    let mut agents = vec![];
    for (i, (tabs, _, _)) in list.iter().enumerate() {
        for (t, surface) in tabs.iter().zip(&surfaces[i]) {
            let Some(a) = &t.agent else { continue };
            let mut p = json!({"surface": surface, "agent": a.agent, "id": a.id});
            if let Some(c) = &t.cwd {
                p["cwd"] = json!(c);
            }
            sub_call(inner, methods::AGENT_SESSION, p).await?;
            let command = a.resume_command();
            if resume {
                sub_call(inner, methods::TERM_SEND, json!({"surface": surface, "text": format!("{command}\n")})).await?;
            }
            agents.push(json!({"surface": surface, "agent": a.agent, "id": a.id, "command": command}));
        }
    }
    inner.tree_changed();
    inner.mark_dirty();
    Ok(json!({"workspace": ws, "folder": folder, "panes": panes, "surfaces": surfaces.concat(), "agents": agents}))
}

async fn split_impl(
    inner: &Arc<Inner>,
    id: u64,
    ws: &str,
    target_pane: &str,
    side: SplitSide,
    kind: &str,
    params: &Value,
) -> RpcResponse {
    let live = if s(params, "cwd").is_none() { live_cwd(inner, target_pane).await } else { None };
    let (cols, rows, cwd) = {
        let state = inner.state.lock().await;
        let (cols, rows) = state
            .workspace(ws)
            .and_then(|w| w.panes.iter().find(|p| p.id == target_pane))
            .and_then(|p| p.surfaces.first())
            .map(|s| match &s.kind {
                SurfaceKind::Terminal { cols, rows, .. } => (*cols, *rows),
                SurfaceKind::Browser { .. } => (120, 30),
            })
            .unwrap_or((120, 30));
        (cols, rows, live.unwrap_or_else(|| default_cwd(params, &state, Some(ws))))
    };

    if kind == "browser" {
        if inner.browser.is_none() && !inner.cfg.app_browser {
            return err(id, RpcErrorCode::Unsupported, "browser backend not available");
        }
        let url = s(params, "url");
        let (surface_ref, pane_ref) = {
            let mut state = inner.state.lock().await;
            let surface_ref = state.next_surface();
            let pane_ref = state.next_pane();
            let surface = browser_surface(surface_ref.clone(), url.as_deref(), browsers_in_app(inner));
            let pane = Pane {
                id: pane_ref.clone(),
                surfaces: vec![surface],
                active_surface: Some(surface_ref.clone()),
            };
            if let Some(w) = state.workspace_mut(ws) {
                w.panes.push(pane);
            }
            state.split_pane(ws, target_pane, side, &pane_ref);
            state.workspace_mut(ws).unwrap().focused_pane = Some(pane_ref.clone());
            (surface_ref, pane_ref)
        };
        if let Err(e) = create_page(inner, &surface_ref, url.as_deref()).await {
            return err(id, RpcErrorCode::Internal, e);
        }
        inner.tree_changed();
        inner.mark_dirty();
        return ok(id, json!({"surface": surface_ref, "pane": pane_ref, "workspace": ws}));
    }

    let (surface_ref, pane_ref, title, command) = {
        let mut state = inner.state.lock().await;
        let surface_ref = state.next_surface();
        let pane_ref = state.next_pane();
        let title = s(params, "name").unwrap_or_else(|| "shell".to_string());
        let surface = terminal_surface(surface_ref.clone(), title.clone(), cols, rows);
        let pane = Pane {
            id: pane_ref.clone(),
            surfaces: vec![surface],
            active_surface: Some(surface_ref.clone()),
        };
        if let Some(w) = state.workspace_mut(ws) {
            w.panes.push(pane);
        }
        state.split_pane(ws, target_pane, side, &pane_ref);
        state.workspace_mut(ws).unwrap().focused_pane = Some(pane_ref.clone());
        (
            surface_ref,
            pane_ref,
            title,
            s(params, "command"),
        )
    };
    if let Err(e) = spawn_terminal(
        inner,
        ws,
        &surface_ref,
        &title,
        &cwd,
        command.as_deref(),
        cols,
        rows,
    )
    .await
    {
        inner.state.lock().await.remove_surface(&surface_ref);
        return err(id, RpcErrorCode::Internal, format!("tmux: {e}"));
    }
    inner.tree_changed();
    inner.mark_dirty();
    ok(id, json!({"pane": pane_ref, "surface": surface_ref}))
}

/// A new tab goes at the end of its pane, or right after the `after` tab when
/// that tab is in the same pane ("New Terminal Tab to Right").
fn place_after(state: &mut crate::tree::SessionState, surface: &str, pane: &str, params: &Value) {
    let Some(after) = s(params, "after") else { return };
    let Some((wi, pi)) = state.locate_pane(pane) else { return };
    let pos = state.workspaces[wi].panes[pi]
        .surfaces
        .iter()
        .filter(|x| x.id != surface)
        .position(|x| x.id == after);
    if let Some(pos) = pos {
        let _ = state.move_surface(surface, pane, Some(pos + 1));
    }
}

async fn add_surface_impl(
    inner: &Arc<Inner>,
    id: u64,
    pane: &str,
    kind: &str,
    params: &Value,
) -> RpcResponse {
    if kind == "browser" {
        if inner.browser.is_none() && !inner.cfg.app_browser {
            return err(id, RpcErrorCode::Unsupported, "browser backend not available");
        }
        let url = s(params, "url");
        let surface_ref = {
            let mut state = inner.state.lock().await;
            let surface_ref = state.next_surface();
            let surface = browser_surface(surface_ref.clone(), url.as_deref(), browsers_in_app(inner));
            state.add_surface(pane, surface);
            place_after(&mut state, &surface_ref, pane, params);
            surface_ref
        };
        if let Err(e) = create_page(inner, &surface_ref, url.as_deref()).await {
            return err(id, RpcErrorCode::Internal, e);
        }
        inner.tree_changed();
        inner.mark_dirty();
        return ok(id, json!({"surface": surface_ref}));
    }

    let live = if s(params, "cwd").is_none() { live_cwd(inner, pane).await } else { None };
    let (surface_ref, ws, title, cwd, command, cols, rows) = {
        let mut state = inner.state.lock().await;
        let (ws, cols, rows) = match state.locate_pane(pane) {
            Some((wi, pi)) => {
                let ws = state.workspaces[wi].id.clone();
                let (c, r) = state.workspaces[wi].panes[pi]
                    .surfaces
                    .first()
                    .map(|s| match &s.kind {
                        SurfaceKind::Terminal { cols, rows, .. } => (*cols, *rows),
                        SurfaceKind::Browser { .. } => (120, 30),
                    })
                    .unwrap_or((120, 30));
                (ws, c, r)
            }
            None => return err(id, RpcErrorCode::NotFound, "pane not found"),
        };
        let surface_ref = state.next_surface();
        let title = s(params, "name").unwrap_or_else(|| "shell".to_string());
        let surface = terminal_surface(surface_ref.clone(), title.clone(), cols, rows);
        state.add_surface(pane, surface);
        place_after(&mut state, &surface_ref, pane, params);
        let cwd = live.unwrap_or_else(|| default_cwd(params, &state, Some(&ws)));
        (
            surface_ref,
            ws,
            title,
            cwd,
            s(params, "command"),
            cols,
            rows,
        )
    };
    if let Err(e) = spawn_terminal(
        inner,
        &ws,
        &surface_ref,
        &title,
        &cwd,
        command.as_deref(),
        cols,
        rows,
    )
    .await
    {
        inner.state.lock().await.remove_surface(&surface_ref);
        return err(id, RpcErrorCode::Internal, format!("tmux: {e}"));
    }
    inner.tree_changed();
    inner.mark_dirty();
    ok(id, json!({"surface": surface_ref}))
}

async fn close_surface(inner: &Arc<Inner>, surface: &str) {
    tracing::info!(%surface, "closing surface (rpc)");
    // Read the term from the guard that is already held, and let it drop before
    // awaiting anything: `surface_term` takes the same lock, and a tokio lock is
    // not re-entrant, so awaiting it here would hang the daemon and every client
    // with it (docs/review-2026-10-09.md, N1).
    let (term, is_browser) = {
        let state = inner.state.lock().await;
        (
            state.term_cloned(surface),
            surface_is_browser(&state, surface),
        )
    };
    if let Some(term) = term {
        inner.kill_closed_terminal(surface, &term.window).await;
        inner.state.lock().await.remove_term(surface);
    }
    if is_browser {
        if let Some(b) = &inner.browser {
            let _ = b.close_surface(surface).await;
        }
    }
    {
        let mut state = inner.state.lock().await;
        state.remove_surface(surface);
    }
    inner.forget_scrollback(surface).await;
    inner
        .send_to_surface(
            surface,
            Out::Msg(ServerMsg::TermExited {
                surface: surface.to_string(),
            }),
        )
        .await;
    inner.tree_changed();
    inner.mark_dirty();
}

async fn close_workspace(inner: &Arc<Inner>, ws: &str) {
    let title = inner.state.lock().await.workspace(ws).map(|w| w.title.clone()).unwrap_or_default();
    tracing::info!(workspace = %ws, %title, "closing workspace (rpc)");
    let surfaces: Vec<(Ref, bool)> = {
        let state = inner.state.lock().await;
        let Some(w) = state.workspace(ws) else {
            return;
        };
        w.panes
            .iter()
            .flat_map(|p| p.surfaces.iter())
            .map(|s| (s.id.clone(), matches!(s.kind, SurfaceKind::Browser { .. })))
            .collect()
    };
    for (surface, is_browser) in surfaces {
        if let Some(term) = inner.surface_term(&surface).await {
            inner.kill_closed_terminal(&surface, &term.window).await;
            inner.state.lock().await.remove_term(&surface);
        }
        inner.forget_scrollback(&surface).await;
        if is_browser {
            if let Some(b) = &inner.browser {
                let _ = b.close_surface(&surface).await;
            }
        }
        inner
            .send_to_surface(
                &surface,
                Out::Msg(ServerMsg::TermExited {
                    surface: surface.clone(),
                }),
            )
            .await;
    }
    {
        let mut state = inner.state.lock().await;
        state.remove_workspace(ws);
    }
    inner.tree_changed();
    inner.mark_dirty();
}

#[cfg(test)]
mod tests {
    use lmux_proto::{BrowserInputEvent as E, KeyAction, MouseAction, MouseButton};

    #[test]
    fn mouse_clicks_are_a_move_then_presses_and_releases() {
        let ev = input_events(methods::BROWSER_MOUSE, &json!({"action": "click", "x": 10, "y": 20})).unwrap();
        assert_eq!(ev.len(), 3);
        assert!(matches!(ev[0], E::Mouse { action: MouseAction::Move, x, y, .. } if x == 10.0 && y == 20.0));
        assert!(matches!(ev[1], E::Mouse { action: MouseAction::Down, button: MouseButton::Left, click_count: 1, .. }));
        assert!(matches!(ev[2], E::Mouse { action: MouseAction::Up, button: MouseButton::Left, click_count: 1, .. }));

        let ev = input_events(methods::BROWSER_MOUSE, &json!({"action": "dblclick", "x": 1, "y": 2, "button": "right"})).unwrap();
        assert_eq!(ev.len(), 5, "move, then two press/release pairs");
        assert!(matches!(ev[3], E::Mouse { action: MouseAction::Down, button: MouseButton::Right, click_count: 2, .. }));

        let ev = input_events(methods::BROWSER_MOUSE, &json!({"action": "wheel", "x": 5, "y": 6, "dy": 120})).unwrap();
        assert!(matches!(ev[0], E::Wheel { delta_y, delta_x, .. } if delta_y == 120.0 && delta_x == 0.0));

        let ev = input_events(methods::BROWSER_MOUSE, &json!({"action": "click", "x": 1, "y": 1, "modifiers": {"shift": true}})).unwrap();
        assert!(matches!(ev[1], E::Mouse { modifiers, .. } if modifiers.shift && !modifiers.ctrl));
    }

    #[test]
    fn mouse_requests_are_checked() {
        assert!(input_events(methods::BROWSER_MOUSE, &json!({"action": "click"})).is_err(), "no coordinates");
        assert!(input_events(methods::BROWSER_MOUSE, &json!({"action": "swipe", "x": 1, "y": 1})).unwrap_err().contains("unknown action"));
        assert!(input_events(methods::BROWSER_MOUSE, &json!({"x": 1, "y": 1, "button": "side"})).unwrap_err().contains("unknown button"));
    }

    #[test]
    fn keyboard_text_and_keys() {
        let ev = input_events(methods::BROWSER_KEYBOARD, &json!({"text": "héllo"})).unwrap();
        assert_eq!(ev, vec![E::InsertText { text: "héllo".into() }]);

        // A character key carries its text; Ctrl and Cmd make it a shortcut.
        let ev = input_events(methods::BROWSER_KEYBOARD, &json!({"key": "a"})).unwrap();
        assert!(matches!(&ev[0], E::Key { action: KeyAction::Down, key, code, text: Some(t), .. } if key == "a" && code == "KeyA" && t == "a"));
        assert!(matches!(&ev[1], E::Key { action: KeyAction::Up, text: None, .. }));
        let ev = input_events(methods::BROWSER_KEYBOARD, &json!({"key": "meta+a"})).unwrap();
        assert!(matches!(&ev[0], E::Key { text: None, modifiers, .. } if modifiers.meta));
        let ev = input_events(methods::BROWSER_KEYBOARD, &json!({"key": "Enter"})).unwrap();
        assert!(matches!(&ev[0], E::Key { key, code, text: None, .. } if key == "Enter" && code == "Enter"));

        assert!(input_events(methods::BROWSER_KEYBOARD, &json!({})).is_err());
    }

    use super::*;
    use crate::tree::{terminal_surface, SessionState};

    fn state() -> SessionState {
        let mut s = SessionState::new("t");
        let ws = s.next_workspace();
        let pane = s.next_pane();
        let surface = s.next_surface();
        s.add_workspace(Workspace {
            id: ws,
            title: "w".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: pane.clone() },
            panes: vec![Pane {
                id: pane.clone(),
                surfaces: vec![terminal_surface(surface.clone(), "t", 80, 24)],
                active_surface: Some(surface.clone()),
            }],
            focused_pane: Some(pane),
            ports: vec![],
            pr: None,
        });
        s
    }

    #[test]
    fn explicit_target_must_exist() {
        let s = state();
        // A bad explicit target is NotFound, not a silent fallback to the caller.
        assert!(resolve_surface(&s, &json!({"surface": "surface:9"})).is_none());
        assert!(resolve_pane(&s, &json!({"pane": "pane:9"})).is_none());
        assert!(resolve_workspace(&s, &json!({"workspace": "workspace:9"})).is_none());
    }

    #[test]
    fn new_tab_after_a_tab() {
        let (mut s, _, pane, term, web) = two_workspaces();
        let new = s.next_surface();
        s.add_surface(&pane, terminal_surface(new.clone(), "n", 80, 24));
        place_after(&mut s, &new, &pane, &json!({"after": term}));
        let (wi, pi) = s.locate_pane(&pane).unwrap();
        let order: Vec<&str> = s.workspaces[wi].panes[pi].surfaces.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(order, [term.as_str(), new.as_str(), web.as_str()]);
        assert_eq!(s.workspaces[wi].panes[pi].active_surface.as_deref(), Some(new.as_str()));
    }

    #[test]
    fn targets_by_number_or_title() {
        let (mut s, ws2, ..) = two_workspaces();
        let norm = |s: &SessionState, p: Value| {
            let mut p = p;
            normalize_targets(s, &mut p).map(|_| p).map_err(|e| e.message)
        };
        let n = ws2.trim_start_matches("workspace:");
        assert_eq!(norm(&s, json!({"workspace": n})).unwrap()["workspace"], ws2.as_str());
        assert_eq!(norm(&s, json!({"workspace": "w2"})).unwrap()["workspace"], ws2.as_str());
        assert_eq!(norm(&s, json!({"surface": "3", "pane": "2"})).unwrap(), json!({"surface": "surface:3", "pane": "pane:2"}));
        // A workspace that is not there says so, rather than "surface not found".
        assert_eq!(norm(&s, json!({"workspace": "workspace:9"})).unwrap_err(), "workspace workspace:9 not found");
        assert_eq!(norm(&s, json!({"workspace": "nope"})).unwrap_err(), "workspace nope not found");
        // Two workspaces with one title: ask for a ref.
        s.workspaces[0].title = "w2".into();
        assert!(norm(&s, json!({"workspace": "w2"})).unwrap_err().contains("2 workspaces are named"));
    }

    #[test]
    fn omitted_target_falls_back_to_caller_or_focus() {
        let s = state();
        assert_eq!(resolve_surface(&s, &json!({})).as_deref(), Some("surface:1"));
        assert_eq!(resolve_pane(&s, &json!({})).as_deref(), Some("pane:1"));
        assert_eq!(resolve_workspace(&s, &json!({})).as_deref(), Some("workspace:1"));
        assert_eq!(
            resolve_surface(&s, &json!({"caller_surface": "surface:1"})).as_deref(),
            Some("surface:1")
        );
    }

    /// `state()` plus a second workspace whose pane holds a terminal and a
    /// browser; returns (state, ws2, pane2, terminal, browser).
    fn two_workspaces() -> (SessionState, Ref, Ref, Ref, Ref) {
        let mut s = state();
        let ws = s.next_workspace();
        let pane = s.next_pane();
        let term = s.next_surface();
        let web = s.next_surface();
        s.add_workspace(Workspace {
            id: ws.clone(),
            title: "w2".into(),
            folder: None,
            cwd: "/tmp".into(),
            git_branch: None,
            agent: AgentState::None,
            summary: None,
            unread_notifications: 0,
            status_pills: vec![],
            progress: None,
            layout: Layout::Pane { pane: pane.clone() },
            panes: vec![Pane {
                id: pane.clone(),
                surfaces: vec![
                    terminal_surface(term.clone(), "t", 80, 24),
                    browser_surface(web.clone(), Some("https://example.com"), false),
                ],
                active_surface: Some(term.clone()),
            }],
            focused_pane: Some(pane.clone()),
            ports: vec![],
            pr: None,
        });
        (s, ws, pane, term, web)
    }

    #[test]
    fn caller_surface_beats_a_stale_workspace_env() {
        let (s, ws2, _, term, _) = two_workspaces();
        // A tab moved to workspace 2 still carries LMUX_WORKSPACE_ID=workspace:1.
        let p = json!({"caller_workspace": "workspace:1", "caller_surface": term});
        assert_eq!(resolve_workspace(&s, &p).as_deref(), Some(ws2.as_str()));
    }

    #[test]
    fn explicit_workspace_beats_the_caller_surface() {
        let (s, ws2, _, term, _) = two_workspaces();
        let p = json!({"workspace": "workspace:1", "caller_surface": term});
        assert_eq!(resolve_surface(&s, &p).as_deref(), Some("surface:1"));
        let p = json!({"workspace": ws2, "caller_surface": "surface:1"});
        assert_eq!(resolve_surface(&s, &p).as_deref(), Some(term.as_str()));
    }

    #[test]
    fn browser_calls_from_a_terminal_find_the_workspace_browser() {
        let (s, _, _, term, web) = two_workspaces();
        let p = json!({"caller_surface": term});
        assert_eq!(resolve_browser_surface(&s, &p).as_deref(), Some(web.as_str()));
        // An explicit surface is never second-guessed.
        let p = json!({"caller_surface": term, "surface": term});
        assert_eq!(resolve_browser_surface(&s, &p).as_deref(), Some(term.as_str()));
        // No browser in the workspace: the terminal stands, and the backend
        // answers for it.
        let p = json!({"caller_surface": "surface:1"});
        assert_eq!(resolve_browser_surface(&s, &p).as_deref(), Some("surface:1"));
    }

    #[test]
    fn move_target_places_next_to_an_anchor() {
        let (s, ws2, pane2, term, web) = two_workspaces();
        // Before the browser, as seen once the moving tab has left the pane.
        let t = move_target(&s, "surface:1", &json!({"before": web})).unwrap();
        assert_eq!(t, (pane2.clone(), Some(1)));
        let t = move_target(&s, "surface:1", &json!({"after": web})).unwrap();
        assert_eq!(t, (pane2.clone(), Some(2)));
        let t = move_target(&s, &term, &json!({"after": web})).unwrap();
        assert_eq!(t, (pane2.clone(), Some(1)));
        // A workspace means its focused pane; nothing means the tab's own pane.
        let t = move_target(&s, "surface:1", &json!({"workspace": ws2, "index": 0})).unwrap();
        assert_eq!(t, (pane2, Some(0)));
        let t = move_target(&s, "surface:1", &json!({})).unwrap();
        assert_eq!(t, ("pane:1".to_string(), None));
        assert!(move_target(&s, "surface:1", &json!({"before": "surface:99"})).is_err());
    }

    #[test]
    fn notify_selectors() {
        let (mut s, ws2, _, term, _) = two_workspaces();
        let n = |id: &str, ws: &str, surface: Option<&str>, read: bool| Notification {
            id: id.into(),
            title: "t".into(),
            subtitle: None,
            body: None,
            workspace: Some(ws.into()),
            surface: surface.map(Into::into),
            created_at: 0,
            read,
        };
        let a = n("a", "workspace:1", None, false);
        let b2 = n("b", &ws2, Some(&term), true);
        let c = n("c", &ws2, None, false);
        let sel = NotifySelector::from_params(&s, &json!({"id": "a"}), "all");
        assert!(sel.matches(&a) && !sel.matches(&b2));
        let sel = NotifySelector::from_params(&s, &json!({"all_read": true}), "all_read");
        assert!(!sel.matches(&a) && sel.matches(&b2));
        // The caller's workspace comes from its surface.
        let sel = NotifySelector::from_params(&s, &json!({"caller_surface": term}), "all");
        assert!(!sel.matches(&a) && sel.matches(&b2) && sel.matches(&c));
        let sel = NotifySelector::from_params(&s, &json!({"workspace": ws2, "surface": term}), "all");
        assert!(sel.matches(&b2) && !sel.matches(&c));
        assert_eq!(NotifySelector::from_params(&s, &json!({}), "all"), NotifySelector::All);

        s.notifications = vec![a, b2, c];
        recount_unread(&mut s);
        assert_eq!(s.workspace("workspace:1").unwrap().unread_notifications, 1);
        assert_eq!(s.workspace(&ws2).unwrap().unread_notifications, 1);
    }

    #[test]
    fn focus_is_restored_after_a_create() {
        let (mut s, ws2, pane2, term, web) = two_workspaces();
        let snap = focus_snapshot(&s, &ws2).unwrap();
        s.workspace_mut(&ws2).unwrap().focused_pane = Some("pane:missing".into());
        s.set_active_surface(&web);
        restore_focus(&mut s, snap);
        let w = s.workspace(&ws2).unwrap();
        assert_eq!(w.focused_pane.as_deref(), Some(pane2.as_str()));
        assert_eq!(w.panes[0].active_surface.as_deref(), Some(term.as_str()));
    }
}
