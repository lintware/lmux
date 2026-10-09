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
    }
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
    if let Some(w) = s(params, "caller_workspace") {
        if state.workspace(&w).is_some() {
            return Some(w);
        }
    }
    if let Some(surf) = s(params, "caller_surface") {
        if let Some(w) = state.workspace_of_surface(&surf) {
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
    if let Some(surf) = s(params, "caller_surface") {
        if state.surface(&surf).is_some() {
            return Some(surf);
        }
    }
    let ws = resolve_workspace(state, params)?;
    state.focused_surface(&ws)
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

async fn spawn_terminal(
    inner: &Arc<Inner>,
    workspace: &str,
    surface: &str,
    title: &str,
    cwd: &str,
    command: Option<&str>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<(String, String)> {
    let env = vec![
        (
            "LMUX_SOCKET".to_string(),
            inner.cfg.socket_path().to_string_lossy().to_string(),
        ),
        ("LMUX_WORKSPACE_ID".to_string(), workspace.to_string()),
        ("LMUX_SURFACE_ID".to_string(), surface.to_string()),
    ];
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

pub async fn dispatch(inner: &Arc<Inner>, req: &RpcRequest) -> RpcResponse {
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
            // (the background timer only refreshes periodically).
            inner.refresh_branches().await;
            inner.tree_changed();
            inner.mark_dirty();
            ok(
                id,
                json!({"workspace": ws_ref, "pane": pane_ref, "surface": surface_ref}),
            )
        }
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
            inner.state.lock().await.selected_workspace = Some(ws.clone());
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
            inner
                .state
                .lock()
                .await
                .move_workspace(&ws, folder, index);
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({"workspace": ws}))
        }

        // --- panes / surfaces ---------------------------------------------
        methods::SPLIT_NEW => {
            let side = parse_side(params);
            let kind = s(params, "type").unwrap_or_else(|| "terminal".to_string());
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "no workspace");
            };
            let target_pane = {
                let state = inner.state.lock().await;
                resolve_pane(&state, params)
            };
            let Some(target_pane) = target_pane else {
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
            ok(id, json!({"panes": panes}))
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
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let pane = {
                let state = inner.state.lock().await;
                resolve_pane(&state, params)
            };
            let (Some(ws), Some(pane)) = (ws, pane) else {
                return err(id, RpcErrorCode::NotFound, "pane not found");
            };
            {
                let mut state = inner.state.lock().await;
                state.resize_pane(&ws, &pane, ratio);
            }
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
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
            };
            let Some(surface) = surface else {
                return err(id, RpcErrorCode::NotFound, "surface not found");
            };
            let Some(target_pane) = s(params, "pane") else {
                return err(id, RpcErrorCode::InvalidParams, "pane required");
            };
            let index = u(params, "index").map(|n| n as usize);
            let res = {
                let mut state = inner.state.lock().await;
                state.move_surface(&surface, &target_pane, index)
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
            let lines = u(params, "lines").map(|n| n as u32);
            match inner.tmux.read_screen(&term.pane, scrollback, lines).await {
                Ok(text) => ok(id, json!({"text": text})),
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
            let ws = s(params, "workspace").or_else(|| s(params, "caller_workspace"));
            {
                let mut state = inner.state.lock().await;
                match ws {
                    Some(ws) => {
                        state
                            .notifications
                            .retain(|n| n.workspace.as_deref() != Some(&ws));
                        if let Some(w) = state.workspace_mut(&ws) {
                            w.unread_notifications = 0;
                        }
                    }
                    None => {
                        state.notifications.clear();
                        for w in &mut state.workspaces {
                            w.unread_notifications = 0;
                        }
                    }
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({}))
        }
        methods::NOTIFY_MARK_READ => {
            let id_param = s(params, "id");
            let ws = s(params, "workspace").or_else(|| s(params, "caller_workspace"));
            {
                let mut state = inner.state.lock().await;
                match id_param {
                    Some(nid) => {
                        if let Some(n) = state.notifications.iter_mut().find(|n| n.id == nid) {
                            n.read = true;
                            let ws = n.workspace.clone();
                            if let Some(ws) = ws {
                                if let Some(w) = state.workspace_mut(&ws) {
                                    w.unread_notifications = w.unread_notifications.saturating_sub(1);
                                }
                            }
                        }
                    }
                    None => {
                        for n in &mut state.notifications {
                            let matches = ws
                                .as_deref()
                                .map(|w| n.workspace.as_deref() == Some(w))
                                .unwrap_or(true);
                            if matches {
                                n.read = true;
                            }
                        }
                        if let Some(ws) = ws {
                            if let Some(w) = state.workspace_mut(&ws) {
                                w.unread_notifications = 0;
                            }
                        } else {
                            for w in &mut state.workspaces {
                                w.unread_notifications = 0;
                            }
                        }
                    }
                }
            }
            inner.tree_changed();
            inner.mark_dirty();
            ok(id, json!({}))
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
            if inner.browser.is_none() {
                return err(id, RpcErrorCode::Unsupported, unsupported(&req.method).message);
            }
            let side = parse_side(params);
            let ws = {
                let state = inner.state.lock().await;
                resolve_workspace(&state, params)
            };
            let Some(ws) = ws else {
                return err(id, RpcErrorCode::NotFound, "no workspace");
            };
            let target_pane = {
                let state = inner.state.lock().await;
                resolve_pane(&state, params)
            };
            let Some(target_pane) = target_pane else {
                return err(id, RpcErrorCode::NotFound, "no target pane");
            };
            split_impl(inner, id, &ws, &target_pane, side, "browser", params).await
        }
        m if m.starts_with("browser.") => {
            let Some(backend) = &inner.browser else {
                return err(id, RpcErrorCode::Unsupported, unsupported(m).message);
            };
            let surface = {
                let state = inner.state.lock().await;
                resolve_surface(&state, params)
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

// A browser tab helper that retains the pane (see browser.open).

async fn split_impl(
    inner: &Arc<Inner>,
    id: u64,
    ws: &str,
    target_pane: &str,
    side: SplitSide,
    kind: &str,
    params: &Value,
) -> RpcResponse {
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
        (cols, rows, default_cwd(params, &state, Some(ws)))
    };

    if kind == "browser" {
        if inner.browser.is_none() {
            return err(id, RpcErrorCode::Unsupported, "browser backend not available");
        }
        let url = s(params, "url");
        let surface_ref = {
            let mut state = inner.state.lock().await;
            let surface_ref = state.next_surface();
            let pane_ref = state.next_pane();
            let surface = Surface {
                id: surface_ref.clone(),
                title: url.clone().unwrap_or_else(|| "browser".to_string()),
                kind: SurfaceKind::Browser {
                    url: url.clone().unwrap_or_else(|| "about:blank".to_string()),
                    loading: false,
                    can_go_back: false,
                    can_go_forward: false,
                },
            };
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
            surface_ref
        };
        let backend = inner.browser.as_ref().unwrap();
        if let Err(e) = backend.create_surface(&surface_ref, url.as_deref()).await {
            inner.state.lock().await.remove_surface(&surface_ref);
            return err(id, RpcErrorCode::Internal, format!("browser: {e}"));
        }
        inner.tree_changed();
        inner.mark_dirty();
        return ok(id, json!({"surface": surface_ref}));
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

async fn add_surface_impl(
    inner: &Arc<Inner>,
    id: u64,
    pane: &str,
    kind: &str,
    params: &Value,
) -> RpcResponse {
    if kind == "browser" {
        if inner.browser.is_none() {
            return err(id, RpcErrorCode::Unsupported, "browser backend not available");
        }
        let url = s(params, "url");
        let surface_ref = {
            let mut state = inner.state.lock().await;
            let surface_ref = state.next_surface();
            let surface = Surface {
                id: surface_ref.clone(),
                title: url.clone().unwrap_or_else(|| "browser".to_string()),
                kind: SurfaceKind::Browser {
                    url: url.clone().unwrap_or_else(|| "about:blank".to_string()),
                    loading: false,
                    can_go_back: false,
                    can_go_forward: false,
                },
            };
            state.add_surface(pane, surface);
            surface_ref
        };
        let backend = inner.browser.as_ref().unwrap();
        if let Err(e) = backend.create_surface(&surface_ref, url.as_deref()).await {
            inner.state.lock().await.remove_surface(&surface_ref);
            return err(id, RpcErrorCode::Internal, format!("browser: {e}"));
        }
        inner.tree_changed();
        inner.mark_dirty();
        return ok(id, json!({"surface": surface_ref}));
    }

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
        let cwd = default_cwd(params, &state, Some(&ws));
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
    let (term, is_browser) = {
        let state = inner.state.lock().await;
        (
            inner.surface_term(surface).await,
            surface_is_browser(&state, surface),
        )
    };
    if let Some(term) = term {
        let _ = inner.tmux.kill_window(&term.window).await;
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
            let _ = inner.tmux.kill_window(&term.window).await;
            inner.state.lock().await.remove_term(&surface);
        }
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
}
