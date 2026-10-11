//! What each CLI command sends to the daemon, and how it prints the answer.

use anyhow::{bail, Context};
use lmux_proto::methods;
use serde_json::{json, Value};

use crate::cli::{BrowserCmd, Cmd, Target};
use crate::{call, wait};

type Printer = Box<dyn Fn(&Value, bool)>;

/// One daemon call and its printer.
struct Req {
    method: &'static str,
    params: Value,
    printer: Printer,
}

fn req(method: &'static str, params: Value, printer: Printer) -> Req {
    Req { method, params, printer }
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()));
}

fn print_json_or(v: &Value, json: bool, human: impl FnOnce(&Value)) {
    if json {
        print_json(v);
    } else {
        human(v);
    }
}

fn refs_line(v: &Value, keys: &[&str]) -> String {
    let parts: Vec<String> = keys
        .iter()
        .filter_map(|k| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string()))
        .collect();
    if parts.is_empty() {
        "OK".to_string()
    } else {
        format!("OK {}", parts.join(" "))
    }
}

fn refs(keys: &'static [&'static str]) -> Printer {
    Box::new(move |v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, keys))))
}

fn ok_box() -> Printer {
    Box::new(|v, j| print_json_or(v, j, |_| println!("OK")))
}

fn text_box() -> Printer {
    Box::new(|v, j| {
        print_json_or(v, j, |v| {
            if let Some(t) = v["text"].as_str() {
                println!("{t}");
            }
        })
    })
}

fn side(s: &str) -> &'static str {
    match s {
        "left" | "l" => "left",
        "up" | "u" => "up",
        "down" | "d" => "down",
        _ => "right",
    }
}

/// Words joined with spaces, as cmux joins trailing text.
fn words(w: &[String]) -> String {
    w.join(" ")
}

/// cmux's `send` escapes: \n and \r press Enter, \t presses Tab.
pub fn unescape_send(text: &str) -> String {
    text.replace("\\n", "\n").replace("\\r", "\n").replace("\\t", "\t")
}

fn target(t: &Target) -> Value {
    json!({"surface": t.surface, "workspace": t.workspace})
}

/// Merge `extra` into `base` (both objects).
fn with(mut base: Value, extra: Value) -> Value {
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    base
}

/// Drop nulls so optional params stay absent.
fn compact(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        other => other,
    }
}

/// `send`/`paste` text takes words that start with a dash (`ls -la`), so clap
/// stops parsing flags at the first word. As in cmux, the command's own flags
/// still count anywhere before a `--` (marked by `cli::mark_end_of_flags`);
/// pull them out of the words here.
fn take_flags(text: &mut Vec<String>, t: &mut Target, bools: &mut [(&str, &mut bool)]) {
    let mut out = Vec::with_capacity(text.len());
    let mut it = std::mem::take(text).into_iter();
    while let Some(w) = it.next() {
        if w == crate::cli::END_OF_FLAGS {
            out.extend(it.by_ref());
            break;
        }
        let (name, inline) = match w.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (w.clone(), None),
        };
        match name.as_str() {
            "--surface" | "--panel" | "--workspace" => {
                let Some(v) = inline.or_else(|| it.next()) else {
                    out.push(w);
                    continue;
                };
                if name == "--workspace" {
                    t.workspace = Some(v);
                } else {
                    t.surface = Some(v);
                }
            }
            _ => match bools.iter_mut().find(|(n, _)| *n == name && inline.is_none()) {
                Some((_, flag)) => **flag = true,
                None => out.push(w),
            },
        }
    }
    *text = out;
}

/// Run a non-server, non-hook command.
pub async fn run(cmd: Cmd, socket: &str, json: bool) -> anyhow::Result<()> {
    // `send`/`paste`: flags written after the text.
    let cmd = match cmd {
        Cmd::Send { mut text, mut target, mut enter, mut paste, mut force } => {
            take_flags(&mut text, &mut target, &mut [("--enter", &mut enter), ("--paste", &mut paste), ("--force", &mut force)]);
            Cmd::Send { text, target, enter, paste, force }
        }
        Cmd::Paste { mut text, mut target, mut submit, mut force } => {
            take_flags(&mut text, &mut target, &mut [("--submit", &mut submit), ("--force", &mut force)]);
            Cmd::Paste { text, target, submit, force }
        }
        other => other,
    };
    // Commands that are more than one plain call.
    match cmd {
        Cmd::Rpc { method, params } => {
            let params: Value = match params.as_deref() {
                None => json!({}),
                Some(p) => serde_json::from_str(p).context("params must be a JSON object")?,
            };
            if !params.is_object() {
                bail!("params must be a JSON object, like '{{\"surface\": \"surface:1\"}}'");
            }
            let v = call(socket, &method, params).await?;
            print_json(&v);
            return Ok(());
        }
        Cmd::WaitFor { name, signal, text, scrollback, target, timeout } => {
            return wait::run(socket, name, signal, text, scrollback, target, timeout).await;
        }
        Cmd::NewWorkspace { name, cwd, command, folder, focus } => {
            let v = call(
                socket,
                methods::WORKSPACE_NEW,
                json!({"name": name, "cwd": cwd, "command": command, "folder": folder}),
            )
            .await?;
            if focus == Some(true) {
                if let Some(ws) = v["workspace"].as_str() {
                    call(socket, methods::WORKSPACE_SELECT, json!({"workspace": ws})).await?;
                }
            }
            print_json_or(&v, json, |v| println!("{}", refs_line(v, &["workspace", "pane", "surface"])));
            return Ok(());
        }
        Cmd::Paste { text, target: t, submit, force: _ } => {
            let text = if text.is_empty() || (text.len() == 1 && text[0] == "-") {
                let mut s = String::new();
                use std::io::Read;
                std::io::stdin().read_to_string(&mut s).context("reading stdin")?;
                s
            } else {
                words(&text)
            };
            if text.is_empty() {
                bail!("paste: nothing to paste");
            }
            let v = call(socket, methods::TERM_SEND, with(target(&t), json!({"text": text}))).await?;
            if submit {
                // The tab that took the paste, even when it was resolved.
                let surface = v["surface"].as_str().map(String::from).or(t.surface.clone());
                call(socket, methods::TERM_SEND_KEY, json!({"surface": surface, "workspace": t.workspace, "key": "enter"}))
                    .await?;
            }
            print_json_or(&v, json, |_| println!("OK"));
            return Ok(());
        }
        Cmd::Notify { clear: true, workspace, surface, .. } => {
            let v = call(socket, methods::NOTIFY_CLEAR, compact(json!({"workspace": workspace, "surface": surface}))).await?;
            print_json_or(&v, json, |_| println!("OK"));
            return Ok(());
        }
        Cmd::DismissNotification { target, id, all_read } => {
            let id = id.or(target);
            let params = match (id.as_ref(), all_read) {
                (Some(id), false) => json!({"id": id}),
                (None, true) => json!({"all_read": true}),
                _ => bail!("dismiss-notification requires exactly one of --id or --all-read"),
            };
            let v = call(socket, methods::NOTIFY_CLEAR, params).await?;
            if let (Some(id), Some(0)) = (id, v["count"].as_u64()) {
                bail!("notification {id} not found");
            }
            print_json_or(&v, json, |_| println!("OK"));
            return Ok(());
        }
        Cmd::MarkNotificationRead { target, id, workspace, surface, all } => {
            let id = id.or(target);
            let n = id.is_some() as u8 + workspace.is_some() as u8 + all as u8;
            if n != 1 {
                bail!("mark-notification-read requires exactly one selector: --id, --workspace, or --all");
            }
            if surface.is_some() && workspace.is_none() {
                bail!("--surface requires --workspace");
            }
            let params = compact(json!({"id": id, "workspace": workspace, "surface": surface, "all": all.then_some(true)}));
            let v = call(socket, methods::NOTIFY_MARK_READ, params).await?;
            if let (Some(id), Some(0)) = (id, v["count"].as_u64()) {
                bail!("notification {id} not found");
            }
            print_json_or(&v, json, |_| println!("OK"));
            return Ok(());
        }
        Cmd::Browser { cmd: BrowserCmd::Open { url, side: _, focus: _, target } }
            if target.surface.is_some() =>
        {
            // `browser <surface> open <url>` navigates, as in cmux.
            let url = words(&url);
            if url.is_empty() {
                bail!("browser <surface> open requires a URL");
            }
            let v = call(socket, methods::BROWSER_GOTO, json!({"surface": target.surface, "url": url})).await?;
            print_json_or(&v, json, |_| println!("OK"));
            return Ok(());
        }
        other => {
            let r = build(other)?;
            let v = call(socket, r.method, r.params).await?;
            (r.printer)(&v, json);
            Ok(())
        }
    }
}

fn build(cmd: Cmd) -> anyhow::Result<Req> {
    Ok(match cmd {
        // ----- workspaces --------------------------------------------------
        Cmd::NewFolder { name, parent } => req(methods::FOLDER_NEW, json!({"name": name, "parent": parent}), refs(&["folder"])),
        Cmd::MoveFolder { folder, parent, index } => req(
            methods::FOLDER_MOVE,
            json!({"folder": folder, "parent": parent, "index": index}),
            ok_box(),
        ),
        Cmd::RenameWorkspace { title, workspace } => req(
            methods::WORKSPACE_RENAME,
            json!({"title": words(&title), "workspace": workspace}),
            refs(&["workspace"]),
        ),
        Cmd::CloseWorkspace { target, workspace, force: _ } => req(
            methods::WORKSPACE_CLOSE,
            json!({"workspace": workspace.or(target)}),
            refs(&["workspace"]),
        ),
        Cmd::SelectWorkspace { target, workspace } => {
            let Some(ws) = workspace.or(target) else {
                bail!("select-workspace requires --workspace <ref>");
            };
            req(methods::WORKSPACE_SELECT, json!({"workspace": ws}), refs(&["workspace"]))
        }
        Cmd::ReorderWorkspace { target, workspace, folder, index, before, after } => {
            let Some(ws) = workspace.or(target) else {
                bail!("reorder-workspace requires --workspace <ref>");
            };
            req(
                methods::WORKSPACE_MOVE,
                compact(json!({"workspace": ws, "folder": folder, "index": index, "before": before, "after": after})),
                refs(&["workspace"]),
            )
        }
        Cmd::ListWorkspaces => req(
            methods::WORKSPACE_LIST,
            json!({}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    for w in v["workspaces"].as_array().cloned().unwrap_or_default() {
                        let agent = w["agent"].as_str().unwrap_or("?");
                        let pills = w["status_pills"].as_array().map(|a| a.len()).unwrap_or(0);
                        println!(
                            "{}\t{}\t{}\t{} pill(s)",
                            w["id"].as_str().unwrap_or("?"),
                            w["title"].as_str().unwrap_or("?"),
                            agent,
                            pills
                        );
                    }
                })
            }),
        ),
        Cmd::CurrentWorkspace => req(
            methods::WORKSPACE_CURRENT,
            json!({}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", v["workspace"].as_str().unwrap_or("")))),
        ),
        Cmd::Tree { workspace } => req(
            methods::TREE,
            json!({}),
            Box::new(move |v, j| {
                let mut v = v.clone();
                if let (Some(ws), Some(list)) = (&workspace, v["workspaces"].as_array_mut()) {
                    list.retain(|w| w["id"].as_str() == Some(ws.as_str()));
                }
                if j {
                    print_json(&v);
                } else {
                    print_tree(&v);
                }
            }),
        ),
        Cmd::Identify { target: t } => req(methods::IDENTIFY, target(&t), refs(&["workspace", "pane", "surface"])),

        // ----- panes & surfaces ----------------------------------------------
        Cmd::NewSplit { side: s, target: t, kind, url, command, cwd, focus } => req(
            methods::SPLIT_NEW,
            compact(with(
                target(&t),
                json!({"side": side(&s), "type": kind, "url": url, "command": command, "cwd": cwd, "focus": focus}),
            )),
            refs(&["pane", "surface"]),
        ),
        Cmd::NewPane { kind, direction, url, command, cwd, target: t, focus } => req(
            methods::SPLIT_NEW,
            compact(with(
                target(&t),
                json!({
                    "side": side(direction.as_deref().unwrap_or("right")),
                    "type": kind, "url": url, "command": command, "cwd": cwd, "focus": focus,
                }),
            )),
            refs(&["pane", "surface"]),
        ),
        Cmd::NewSurface { pane, workspace, kind, url, command, cwd, focus } => req(
            methods::SURFACE_NEW,
            compact(json!({
                "pane": pane, "workspace": workspace, "type": kind, "url": url,
                "command": command, "cwd": cwd, "focus": focus,
            })),
            refs(&["surface"]),
        ),
        Cmd::CloseSurface { target: t, force: _ } => req(methods::SURFACE_CLOSE, target(&t), refs(&["surface"])),
        Cmd::RenameTab { words: w, title, surface, workspace } => {
            let title = title.unwrap_or_else(|| words(&w));
            if title.trim().is_empty() {
                bail!("rename-tab requires a title");
            }
            req(
                methods::SURFACE_RENAME,
                json!({"title": title, "surface": surface, "workspace": workspace}),
                refs(&["surface"]),
            )
        }
        Cmd::MoveSurface { target, surface, pane, workspace, index, before, after, focus: _ } => {
            let Some(surface) = surface.or(target) else {
                bail!("move-surface requires --surface <ref>");
            };
            if pane.is_none() && workspace.is_none() && before.is_none() && after.is_none() && index.is_none() {
                bail!("move-surface needs a destination: --pane, --workspace, --before, --after or --index");
            }
            req(
                methods::SURFACE_MOVE,
                compact(json!({
                    "surface": surface, "pane": pane, "workspace": workspace,
                    "index": index, "before": before, "after": after,
                })),
                refs(&["surface", "pane", "workspace"]),
            )
        }
        Cmd::ReorderSurface { target, surface, index, before, after } => {
            let Some(surface) = surface.or(target) else {
                bail!("reorder-surface requires --surface <ref>");
            };
            if index.is_none() && before.is_none() && after.is_none() {
                bail!("reorder-surface needs --index, --before or --after");
            }
            req(
                methods::SURFACE_MOVE,
                compact(json!({"surface": surface, "index": index, "before": before, "after": after})),
                refs(&["surface", "pane"]),
            )
        }
        Cmd::SplitOff { args, surface, side: side_flag, pane, focus: _ } => {
            // cmux: `split-off --surface S <side>`; lmux: `split-surface S --side <side>`.
            let mut pos = args.into_iter();
            let surface = surface.or_else(|| {
                if side_flag.is_some() || pos.len() == 2 {
                    pos.next()
                } else {
                    None
                }
            });
            let Some(surface) = surface else {
                bail!("split-off requires --surface <ref>");
            };
            let Some(s) = side_flag.or_else(|| pos.next()) else {
                bail!("split-off requires a side: left, right, up or down");
            };
            if !["left", "right", "up", "down", "l", "r", "u", "d"].contains(&s.as_str()) {
                bail!("split-off: side must be left, right, up or down, not {s:?}");
            }
            req(
                methods::SURFACE_SPLIT,
                json!({"surface": surface, "side": side(&s), "pane": pane}),
                refs(&["surface", "pane"]),
            )
        }
        Cmd::MoveTabToNewWorkspace { surface, workspace, title, focus } => req(
            methods::SURFACE_MOVE_TO_NEW_WORKSPACE,
            compact(json!({"surface": surface, "workspace": workspace, "title": title, "focus": focus})),
            refs(&["workspace", "pane", "surface"]),
        ),
        Cmd::SwapPane { panes, pane, target_pane, focus: _ } => {
            let mut pos = panes.into_iter();
            let (Some(a), Some(b)) = (pane.or_else(|| pos.next()), target_pane.or_else(|| pos.next())) else {
                bail!("swap-pane requires --pane <ref> --target-pane <ref>");
            };
            req(methods::PANE_SWAP, json!({"pane": a, "other": b}), ok_box())
        }
        Cmd::FocusPane { target, pane, workspace: _ } => {
            let Some(pane) = pane.or(target) else {
                bail!("focus-pane requires --pane <ref>");
            };
            req(methods::PANE_FOCUS, json!({"pane": pane}), refs(&["pane", "workspace"]))
        }
        Cmd::ResizePane { target, pane, ratio } => req(
            methods::PANE_RESIZE,
            json!({"pane": pane.or(target), "ratio": ratio}),
            ok_box(),
        ),
        Cmd::ListPanes { workspace } => req(
            methods::PANE_LIST,
            json!({"workspace": workspace}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    let panes = v["panes"].as_array().cloned().unwrap_or_default();
                    if panes.is_empty() {
                        println!("No panes");
                    }
                    for p in panes {
                        let focused = p["id"] == v["focused_pane"];
                        let n = p["surfaces"].as_array().map(|a| a.len()).unwrap_or(0);
                        println!(
                            "{}{}  [{n} surface{}]{}",
                            if focused { "* " } else { "  " },
                            p["id"].as_str().unwrap_or("?"),
                            if n == 1 { "" } else { "s" },
                            if focused { "  [focused]" } else { "" },
                        );
                    }
                })
            }),
        ),
        Cmd::ListPaneSurfaces { pane, workspace } => req(
            methods::PANE_SURFACES,
            json!({"pane": pane, "workspace": workspace}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    let surfaces = v["surfaces"].as_array().cloned().unwrap_or_default();
                    if surfaces.is_empty() {
                        println!("No surfaces in pane");
                    }
                    for s in surfaces {
                        let selected = s["selected"].as_bool().unwrap_or(false);
                        let url = s["url"].as_str().map(|u| format!("  <{u}>")).unwrap_or_default();
                        println!(
                            "{}{}  {}{url}{}",
                            if selected { "* " } else { "  " },
                            s["id"].as_str().unwrap_or("?"),
                            s["title"].as_str().unwrap_or(""),
                            if selected { "  [selected]" } else { "" },
                        );
                    }
                })
            }),
        ),

        // ----- terminal ------------------------------------------------------
        Cmd::Send { text, target: t, enter, paste, force: _ } => {
            let raw = words(&text);
            if raw.is_empty() && !enter {
                bail!("send requires text");
            }
            let mut text = if paste { raw } else { unescape_send(&raw) };
            if enter {
                text.push('\n');
            }
            req(methods::TERM_SEND, with(target(&t), json!({"text": text})), ok_box())
        }
        Cmd::SendKey { key, target: t, force: _ } => {
            req(methods::TERM_SEND_KEY, with(target(&t), json!({"key": key})), ok_box())
        }
        Cmd::ReadScreen { target: t, scrollback, lines } => req(
            methods::TERM_READ_SCREEN,
            with(target(&t), json!({"scrollback": scrollback || lines.is_some(), "lines": lines})),
            text_box(),
        ),
        Cmd::CapturePane { target: t, tmux_target, print: _, start, end, join, escape, scrollback, lines } => {
            let t = Target { surface: t.surface.or(tmux_target), ..t };
            req(
                methods::TERM_READ_SCREEN,
                compact(with(
                    target(&t),
                    json!({
                        "scrollback": scrollback || lines.is_some(), "lines": lines,
                        "start": start, "end": end,
                        "join": join.then_some(true), "escape": escape.then_some(true),
                    }),
                )),
                text_box(),
            )
        }
        Cmd::ClearHistory { target: t, tmux_target } => {
            let t = Target { surface: t.surface.or(tmux_target), ..t };
            req(methods::TERM_CLEAR_HISTORY, target(&t), refs(&["surface"]))
        }

        // ----- notifications ---------------------------------------------------
        Cmd::Notify { title_arg, title, subtitle, body, workspace, surface, clear: _ } => req(
            methods::NOTIFY,
            json!({
                "title": title.or(title_arg).unwrap_or_else(|| "Notification".into()),
                "subtitle": subtitle, "body": body, "workspace": workspace, "surface": surface,
            }),
            refs(&["id"]),
        ),
        Cmd::ListNotifications => req(
            methods::NOTIFY_LIST,
            json!({}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    let list = v["notifications"].as_array().cloned().unwrap_or_default();
                    if list.is_empty() {
                        println!("No notifications");
                    }
                    // cmux's line: index:id|workspace|surface|read|title|subtitle|body
                    for (i, n) in list.iter().enumerate() {
                        let f = |k: &str| n[k].as_str().unwrap_or("").replace('\n', " ");
                        println!(
                            "{i}:{}|{}|{}|{}|{}|{}|{}",
                            f("id"),
                            f("workspace"),
                            n["surface"].as_str().unwrap_or("none"),
                            if n["read"].as_bool().unwrap_or(false) { "read" } else { "unread" },
                            f("title"),
                            f("subtitle"),
                            f("body"),
                        );
                    }
                })
            }),
        ),
        Cmd::ClearNotifications { workspace, surface } => {
            if surface.is_some() && workspace.is_none() {
                bail!("clear-notifications --surface requires --workspace");
            }
            req(methods::NOTIFY_CLEAR, compact(json!({"workspace": workspace, "surface": surface})), ok_box())
        }
        Cmd::OpenNotification { target, id } => {
            let Some(id) = id.or(target) else {
                bail!("open-notification requires --id");
            };
            req(methods::NOTIFY_JUMP_TO_UNREAD, json!({"id": id}), refs(&["workspace", "surface"]))
        }
        Cmd::JumpToUnread => req(
            methods::NOTIFY_JUMP_TO_UNREAD,
            json!({}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    if v.get("id").is_none() {
                        println!("No unread notifications");
                    } else {
                        println!("{}", refs_line(v, &["workspace", "surface"]));
                    }
                })
            }),
        ),

        // ----- status ------------------------------------------------------------
        Cmd::SetStatus { key, value, icon, color, priority: _, workspace } => req(
            methods::STATUS_SET,
            json!({"key": key, "value": value, "icon": icon, "color": color, "workspace": workspace}),
            ok_box(),
        ),
        Cmd::ClearStatus { key, workspace } => {
            req(methods::STATUS_CLEAR, json!({"key": key, "workspace": workspace}), ok_box())
        }
        Cmd::ListStatus { workspace } => req(
            methods::STATUS_LIST,
            json!({"workspace": workspace}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    let pills = v["status"].as_array().cloned().unwrap_or_default();
                    if pills.is_empty() {
                        println!("No status entries");
                    }
                    for p in pills {
                        let mut line = format!(
                            "{}={}",
                            p["key"].as_str().unwrap_or(""),
                            p["value"].as_str().unwrap_or("")
                        );
                        for k in ["icon", "color"] {
                            if let Some(x) = p[k].as_str() {
                                line.push_str(&format!(" {k}={x}"));
                            }
                        }
                        println!("{line}");
                    }
                })
            }),
        ),
        Cmd::SetProgress { value, label, workspace } => req(
            methods::PROGRESS_SET,
            json!({"value": value, "label": label, "workspace": workspace}),
            ok_box(),
        ),
        Cmd::ClearProgress { workspace } => req(methods::PROGRESS_CLEAR, json!({"workspace": workspace}), ok_box()),
        Cmd::Log { message, level, source, workspace } => req(
            methods::LOG,
            json!({"message": words(&message), "level": level, "source": source, "workspace": workspace}),
            ok_box(),
        ),
        Cmd::ListLog { workspace, limit } => req(
            methods::LOG_LIST,
            json!({"workspace": workspace, "limit": limit}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    let entries = v["entries"].as_array().cloned().unwrap_or_default();
                    if entries.is_empty() {
                        println!("No log entries");
                    }
                    for e in entries {
                        let mut line =
                            format!("[{}] {}", e["level"].as_str().unwrap_or("info"), e["message"].as_str().unwrap_or(""));
                        if let Some(src) = e["source"].as_str().filter(|s| !s.is_empty()) {
                            line = format!("[{src}] {line}");
                        }
                        println!("{line}");
                    }
                })
            }),
        ),
        Cmd::ClearLog { workspace: _ } => req(methods::LOG_CLEAR, json!({}), ok_box()),

        Cmd::Ping => req(
            methods::PING,
            json!({}),
            Box::new(|v, j| print_json_or(v, j, |_| println!("PONG"))),
        ),
        Cmd::Browser { cmd } => build_browser(cmd)?,
        Cmd::Server(_)
        | Cmd::Hooks { .. }
        | Cmd::WsProbe { .. }
        | Cmd::Share { .. }
        | Cmd::ShareBackend
        | Cmd::ShareCode { .. }
        | Cmd::Rpc { .. }
        | Cmd::ImportCmux { .. }
        | Cmd::WaitFor { .. }
        | Cmd::NewWorkspace { .. }
        | Cmd::Paste { .. }
        | Cmd::DismissNotification { .. }
        | Cmd::MarkNotificationRead { .. } => unreachable!("handled before build"),
    })
}

/// A browser target: an explicit selector flag beats the positional one.
fn pick(flag: Option<String>, pos: Option<String>, what: &str) -> anyhow::Result<String> {
    flag.or(pos).with_context(|| format!("a {what} is required"))
}

fn build_browser(cmd: BrowserCmd) -> anyhow::Result<Req> {
    Ok(match cmd {
        BrowserCmd::Open { url, side, focus, target } | BrowserCmd::OpenSplit { url, side, focus, target } => {
            let url = words(&url);
            req(
                methods::BROWSER_OPEN,
                compact(json!({
                    "url": (!url.is_empty()).then_some(url), "side": side,
                    "surface": target.surface, "workspace": target.workspace, "focus": focus,
                })),
                refs(&["surface", "pane", "workspace"]),
            )
        }
        BrowserCmd::Goto { url, target } => {
            req(methods::BROWSER_GOTO, json!({"url": words(&url), "surface": target.surface, "workspace": target.workspace}), ok_box())
        }
        BrowserCmd::Back { target } => req(methods::BROWSER_BACK, json!({"surface": target.surface, "workspace": target.workspace}), ok_box()),
        BrowserCmd::Forward { target } => req(methods::BROWSER_FORWARD, json!({"surface": target.surface, "workspace": target.workspace}), ok_box()),
        BrowserCmd::Reload { target } => req(methods::BROWSER_RELOAD, json!({"surface": target.surface, "workspace": target.workspace}), ok_box()),
        BrowserCmd::Url { target } => req(
            methods::BROWSER_URL,
            json!({"surface": target.surface, "workspace": target.workspace}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    println!("{}\t{}", v["url"].as_str().unwrap_or(""), v["title"].as_str().unwrap_or(""))
                })
            }),
        ),
        BrowserCmd::Snapshot { interactive, target } => req(
            methods::BROWSER_SNAPSHOT,
            json!({"surface": target.surface, "workspace": target.workspace, "interactive": interactive}),
            text_box(),
        ),
        BrowserCmd::Click { selector_arg, selector, target } => req(
            methods::BROWSER_CLICK,
            json!({"target": pick(selector, selector_arg, "target")?, "surface": target.surface, "workspace": target.workspace}),
            ok_box(),
        ),
        BrowserCmd::Dblclick { selector_arg, selector, target } => req(
            methods::BROWSER_DBLCLICK,
            json!({"target": pick(selector, selector_arg, "target")?, "surface": target.surface, "workspace": target.workspace}),
            ok_box(),
        ),
        BrowserCmd::Hover { selector_arg, selector, target } => req(
            methods::BROWSER_HOVER,
            json!({"target": pick(selector, selector_arg, "target")?, "surface": target.surface, "workspace": target.workspace}),
            ok_box(),
        ),
        BrowserCmd::Type { args, selector, text, target } => {
            let (sel, text) = selector_and_text(args, selector, text, "text")?;
            req(methods::BROWSER_TYPE, json!({"target": sel, "text": text, "surface": target.surface, "workspace": target.workspace}), ok_box())
        }
        BrowserCmd::Fill { args, selector, text, target } => {
            let (sel, text) = selector_and_text(args, selector, text, "text")?;
            req(methods::BROWSER_FILL, json!({"target": sel, "text": text, "surface": target.surface, "workspace": target.workspace}), ok_box())
        }
        BrowserCmd::Press { key_arg, key, target } => req(
            methods::BROWSER_PRESS,
            json!({"key": pick(key, key_arg, "key")?, "surface": target.surface, "workspace": target.workspace}),
            ok_box(),
        ),
        BrowserCmd::Mouse { action, x, y, button, dx, dy, shift, ctrl, alt, meta, target } => req(
            methods::BROWSER_MOUSE,
            compact(json!({
                "action": action, "x": x, "y": y, "button": button, "dx": dx, "dy": dy,
                "modifiers": {"shift": shift, "ctrl": ctrl, "alt": alt, "meta": meta},
                "surface": target.surface, "workspace": target.workspace,
            })),
            ok_box(),
        ),
        BrowserCmd::Keyboard { text, key, target } => {
            if text.is_none() && key.is_none() {
                bail!("keyboard needs --text to type or --key to press");
            }
            req(
                methods::BROWSER_KEYBOARD,
                compact(json!({"text": text, "key": key, "surface": target.surface, "workspace": target.workspace})),
                ok_box(),
            )
        }
        BrowserCmd::Select { args, selector, value, target } => {
            let (sel, value) = selector_and_text(args, selector, value, "value")?;
            req(methods::BROWSER_SELECT, json!({"target": sel, "value": value, "surface": target.surface, "workspace": target.workspace}), ok_box())
        }
        BrowserCmd::Scroll { selector_arg, selector, dx, dy, target } => req(
            methods::BROWSER_SCROLL,
            json!({"target": selector.or(selector_arg), "dx": dx, "dy": dy, "surface": target.surface, "workspace": target.workspace}),
            ok_box(),
        ),
        BrowserCmd::Wait { selector, text, url_contains, load_state, timeout_ms, timeout, target } => req(
            methods::BROWSER_WAIT,
            json!({
                "selector": selector, "text": text, "url_contains": url_contains, "load_state": load_state,
                "timeout_ms": timeout_ms.or(timeout.map(|s| (s * 1000.0) as u64)), "surface": target.surface, "workspace": target.workspace,
            }),
            ok_box(),
        ),
        BrowserCmd::Eval { script_args, script, target } => {
            let script = script.unwrap_or_else(|| words(&script_args));
            if script.is_empty() {
                bail!("eval requires a script");
            }
            req(
                methods::BROWSER_EVAL,
                json!({"script": script, "surface": target.surface, "workspace": target.workspace}),
                Box::new(|v, j| print_json_or(v, j, |v| println!("{}", v["value"]))),
            )
        }
        BrowserCmd::Screenshot { path, target } => req(
            methods::BROWSER_SCREENSHOT,
            json!({"path": path, "surface": target.surface, "workspace": target.workspace}),
            refs(&["path"]),
        ),
        BrowserCmd::Get { what, args, target_selector, name, target } => {
            let mut pos = args.into_iter();
            let sel = target_selector.or_else(|| pos.next());
            let name = name.or_else(|| pos.next());
            req(
                methods::BROWSER_GET,
                json!({"what": what, "target": sel, "name": name, "surface": target.surface, "workspace": target.workspace}),
                Box::new(|v, j| {
                    print_json_or(v, j, |v| match &v["value"] {
                        Value::String(s) => println!("{s}"),
                        other => println!("{other}"),
                    })
                }),
            )
        }
        BrowserCmd::Console { action, clear, target } => req(
            methods::BROWSER_CONSOLE,
            json!({"clear": clear || action.as_deref() == Some("clear"), "surface": target.surface, "workspace": target.workspace}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", v["entries"]))),
        ),
        BrowserCmd::Errors { action, clear, target } => req(
            methods::BROWSER_ERRORS,
            json!({"clear": clear || action.as_deref() == Some("clear"), "surface": target.surface, "workspace": target.workspace}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", v["entries"]))),
        ),
        BrowserCmd::Tab { action, arg, index, url, target } => {
            // `tab 2` switches to tab 2; `tab new <url>`; `tab switch 2`.
            let (action, index, url) = match action.parse::<u64>() {
                Ok(n) => ("switch".to_string(), Some(n), url),
                Err(_) if action == "new" => (action, index, url.or(arg)),
                Err(_) => {
                    let n = index.or(arg.as_deref().and_then(|a| a.parse().ok()));
                    (action, n, url)
                }
            };
            req(
                methods::BROWSER_TABS,
                json!({"action": action, "index": index, "url": url, "surface": target.surface, "workspace": target.workspace}),
                Box::new(|v, j| {
                    if j || v.as_object().is_some_and(|o| !o.is_empty()) {
                        print_json(v);
                    } else {
                        println!("OK");
                    }
                }),
            )
        }
    })
}

/// `type`/`fill`/`select`: the target from --selector or the first word, the
/// text from the flag or the remaining words.
fn selector_and_text(
    args: Vec<String>,
    selector: Option<String>,
    text: Option<String>,
    what: &str,
) -> anyhow::Result<(String, String)> {
    let mut pos = args.into_iter();
    let sel = match selector {
        Some(s) => s,
        None => pos.next().context("a target (element ref or CSS selector) is required")?,
    };
    let rest: Vec<String> = pos.collect();
    let text = match text {
        Some(t) => t,
        None if !rest.is_empty() => words(&rest),
        None => bail!("a {what} is required"),
    };
    Ok((sel, text))
}

fn print_tree(tree: &Value) {
    let host = tree["host"].as_str().unwrap_or("?");
    println!("host {host}");
    let selected = tree["selected_workspace"].as_str().unwrap_or("");
    for w in tree["workspaces"].as_array().cloned().unwrap_or_default() {
        let mark = if w["id"].as_str() == Some(selected) { "*" } else { " " };
        println!(
            "{mark} {} \"{}\" [{}] cwd={}",
            w["id"].as_str().unwrap_or("?"),
            w["title"].as_str().unwrap_or("?"),
            w["agent"].as_str().unwrap_or("none"),
            w["cwd"].as_str().unwrap_or("")
        );
        print_layout(&w, &w["layout"], 2);
    }
}

fn print_layout(ws: &Value, layout: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match layout["type"].as_str() {
        Some("pane") => {
            let id = layout["pane"].as_str().unwrap_or("?");
            let focused = ws["focused_pane"].as_str() == Some(id);
            println!("{pad}pane {id}{}", if focused { " [focused]" } else { "" });
            let pane = ws["panes"].as_array().and_then(|ps| ps.iter().find(|p| p["id"].as_str() == Some(id)));
            if let Some(p) = pane {
                for s in p["surfaces"].as_array().cloned().unwrap_or_default() {
                    let sid = s["id"].as_str().unwrap_or("?");
                    let active = p["active_surface"].as_str() == Some(sid);
                    let url = s["kind"]["url"].as_str().map(|u| format!(" <{u}>")).unwrap_or_default();
                    println!(
                        "{pad}  surface {sid} \"{}\"{url}{}",
                        s["title"].as_str().unwrap_or(""),
                        if active { " [selected]" } else { "" }
                    );
                }
            }
        }
        Some("split") => {
            println!(
                "{pad}split {} ratio={}",
                layout["direction"].as_str().unwrap_or("?"),
                layout["ratio"].as_f64().unwrap_or(0.5)
            );
            print_layout(ws, &layout["first"], indent + 2);
            print_layout(ws, &layout["second"], indent + 2);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_escapes_match_cmux() {
        assert_eq!(unescape_send("ls -la\\n"), "ls -la\n");
        assert_eq!(unescape_send("a\\tb\\r"), "a\tb\n");
        assert_eq!(unescape_send("plain"), "plain");
    }

    #[test]
    fn flags_after_send_text_still_count() {
        let mut text: Vec<String> = "ls -la --surface surface:3 --enter END --surface x"
            .split(' ')
            .map(|w| if w == "END" { crate::cli::END_OF_FLAGS.to_string() } else { w.to_string() })
            .collect();
        let mut t = Target::default();
        let (mut enter, mut paste) = (false, false);
        take_flags(&mut text, &mut t, &mut [("--enter", &mut enter), ("--paste", &mut paste)]);
        assert_eq!(text, vec!["ls", "-la", "--surface", "x"]);
        assert_eq!(t.surface.as_deref(), Some("surface:3"));
        assert!(enter && !paste);

        let mut text = vec!["--workspace=workspace:2".to_string(), "hi".into()];
        take_flags(&mut text, &mut t, &mut []);
        assert_eq!(text, vec!["hi"]);
        assert_eq!(t.workspace.as_deref(), Some("workspace:2"));
    }

    #[test]
    fn type_takes_selector_then_text() {
        let a = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(
            selector_and_text(a("#q hello world"), None, None, "text").unwrap(),
            ("#q".into(), "hello world".into())
        );
        assert_eq!(
            selector_and_text(a("hello"), Some("#q".into()), None, "text").unwrap(),
            ("#q".into(), "hello".into())
        );
        assert!(selector_and_text(a("#q"), None, None, "text").is_err());
    }
}
