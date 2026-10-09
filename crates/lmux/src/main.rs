//! The `lmux` binary: `lmux server` runs the daemon; every other command is a
//! thin JSON-RPC client over the daemon's unix socket.

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{CommandFactory, FromArgMatches};
use lmux_proto::{methods, ClientMsg, RpcRequest, RpcResponse, ServerMsg};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

mod browser_adapter;
mod cli;
mod commands;
mod import;
mod wait;

use cli::{ClaudeHook, Cli, Cmd, HooksCmd};

/// Parse argv, first moving a cmux-style `browser <surface> <sub>` surface
/// after the subcommand.
fn parse_cli() -> Cli {
    let command = Cli::command();
    let browser_subs: Vec<String> = command
        .find_subcommand("browser")
        .map(|b| {
            b.get_subcommands()
                .flat_map(|s| std::iter::once(s.get_name().to_string()).chain(s.get_all_aliases().map(String::from)))
                .chain(["help".to_string()])
                .collect()
        })
        .unwrap_or_default();
    let args = cli::rewrite_browser_surface(std::env::args().collect(), &browser_subs);
    let args = cli::mark_end_of_flags(args);
    let matches = command.get_matches_from(args);
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

fn default_socket() -> String {
    if let Ok(s) = std::env::var("LMUX_SOCKET") {
        if !s.is_empty() {
            return s;
        }
    }
    lmux_core::default_state_dir()
        .join("lmux.sock")
        .to_string_lossy()
        .to_string()
}

#[tokio::main]
async fn main() {
    // Rust ignores SIGPIPE, which makes `lmux … | head` panic on a closed pipe.
    // Restore the default so piping behaves like any other Unix tool.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if let Err(e) = run().await {
        eprintln!("lmux: {e:#}");
        std::process::exit(1);
    }
}

/// Resolves on SIGINT or SIGTERM, so `lmux server` can stop Chromium before it
/// exits (the daemon only targets unix: it needs tmux).
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = parse_cli();
    let socket = cli.socket.clone().unwrap_or_else(default_socket);
    let json = cli.json;

    match cli.cmd {
        Cmd::Server(args) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();
            let cfg = lmux_core::Config {
                state_dir: args.state_dir.unwrap_or_else(lmux_core::default_state_dir),
                // `lmux --socket PATH server` listens where clients will look.
                socket: cli.socket.clone().map(PathBuf::from),
                tmux_socket: args.tmux_socket,
                ws_addr: args.ws_addr,
                host: lmux_core::daemon::hostname(),
                app_browser: args.app_browser,
            };
            // Browser backend is wired in here: this is the only crate that
            // sees both the daemon's trait and the engine.
            // WebKit when a helper is installed (as cmux, but on the host),
            // unless Chromium is asked for.
            let webkit = args.webkit_helper.clone().or_else(|| {
                let p = dirs_home()?.join(".local/libexec/lmux/lmux-webkit");
                (!args.chromium && args.chromium_path.is_none() && p.is_file()).then_some(p)
            });
            let browser: Option<std::sync::Arc<dyn lmux_core::BrowserBackend>> = if args.no_browser || args.app_browser {
                tracing::info!("no browser engine here (--no-browser or --app-browser)");
                None
            } else if let Some(helper) = webkit {
                match lmux_core::webkit::WebKitBackend::launch(helper.clone()).await {
                    Ok(b) => {
                        tracing::info!(helper = %helper.display(), "browser backend ready (WebKit)");
                        Some(std::sync::Arc::new(b) as std::sync::Arc<dyn lmux_core::BrowserBackend>)
                    }
                    Err(e) => {
                        tracing::warn!("WebKit helper unavailable ({e}); terminals only");
                        None
                    }
                }
            } else {
                match browser_adapter::ChromiumBackend::launch(args.chromium_path.clone()).await {
                    Ok(backend) => {
                        let backend = std::sync::Arc::new(backend);
                        // A signal kills this process without dropping the
                        // child, which would orphan Chromium, so shut it down
                        // explicitly first.
                        let for_signal = backend.clone();
                        tokio::spawn(async move {
                            shutdown_signal().await;
                            tracing::info!("shutting down browser");
                            for_signal.shutdown().await;
                            std::process::exit(0);
                        });
                        tracing::info!("browser backend ready");
                        Some(backend as std::sync::Arc<dyn lmux_core::BrowserBackend>)
                    }
                    Err(e) => {
                        tracing::warn!("browser backend unavailable ({e}); terminals only");
                        None
                    }
                }
            };
            lmux_core::run(cfg, browser).await?;
            return Ok(());
        }
        Cmd::WsProbe { surface, addr, timeout, resizes } => {
            ws_probe(&addr, &surface, timeout, resizes).await?;
            return Ok(());
        }
        Cmd::ImportCmux { move_, force, dry_run, only, from } => {
            let opts = import::Opts { from, move_, force, dry_run, only };
            return import::run(opts, &socket, json).await;
        }
        Cmd::Hooks { cmd } => {
            hooks(cmd, &socket, json).await?;
            return Ok(());
        }
        other => commands::run(other, &socket, json).await,
    }
}

// ---------------------------------------------------------------------------
// RPC client
// ---------------------------------------------------------------------------

async fn call(socket: &str, method: &str, mut params: Value) -> anyhow::Result<Value> {
    if let Some(obj) = params.as_object_mut() {
        if let Ok(w) = std::env::var("LMUX_WORKSPACE_ID") {
            obj.insert("caller_workspace".into(), json!(w));
        }
        if let Ok(s) = std::env::var("LMUX_SURFACE_ID") {
            obj.insert("caller_surface".into(), json!(s));
        }
        obj.insert("caller_pid".into(), json!(std::process::id()));
    }
    let req = RpcRequest {
        id: 1,
        method: method.to_string(),
        params,
    };
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to daemon socket {socket} (is `lmux server` running?)"))?;
    call_on(stream, req).await
}

/// Like [`call`], but gives up after `limit`. Claude Code waits for hooks, so a
/// hook must never hang on a daemon that is down or wedged.
async fn call_quick(socket: &str, method: &str, params: Value, limit: std::time::Duration) {
    let _ = tokio::time::timeout(limit, call(socket, method, params)).await;
}

async fn call_on(stream: tokio::net::UnixStream, req: RpcRequest) -> anyhow::Result<Value> {
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut reader = BufReader::new(r);
    let mut resp_line = String::new();
    let n = reader.read_line(&mut resp_line).await?;
    if n == 0 {
        bail!("daemon closed the connection without a reply");
    }
    let resp: RpcResponse = serde_json::from_str(resp_line.trim())
        .with_context(|| format!("parsing response: {}", resp_line.trim()))?;
    if let Some(e) = resp.error {
        bail!("{} ({:?})", e.message, e.code);
    }
    Ok(resp.result.unwrap_or_else(|| json!({})))
}

// ---------------------------------------------------------------------------
// Claude Code hooks
// ---------------------------------------------------------------------------

async fn hooks(cmd: HooksCmd, socket: &str, _json: bool) -> anyhow::Result<()> {
    let HooksCmd::Claude { cmd } = cmd;
    if matches!(cmd, ClaudeHook::PrintConfig) {
        println!("{}", claude_settings_snippet());
        return Ok(());
    }

    let mut input = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut input).ok();
    let payload: Value = serde_json::from_str(&input).unwrap_or(json!({}));
    // Only panes lmux started report agent state. No explicit workspace is
    // sent: the daemon resolves the caller's surface, which follows a moved tab.
    let ws = std::env::var("LMUX_WORKSPACE_ID").ok();
    let summary = |payload: &Value| -> Option<String> {
        summarize_transcript(payload)
    };

    match cmd {
        ClaudeHook::SessionStart => {
            // Remember the session so the pane can run `claude --resume` after a
            // reboot. Subagents report their own SessionStart; only the
            // top-level session resumes. `/clear`, `/compact` and fork start a
            // new session id, which this records too.
            let is_subagent = payload.get("agent_id").is_some_and(|v| !v.is_null());
            if let (Ok(surface), Some(id), false) = (
                std::env::var("LMUX_SURFACE_ID"),
                payload["session_id"].as_str().filter(|s| !s.is_empty()),
                is_subagent,
            ) {
                call_quick(
                    socket,
                    methods::AGENT_SESSION,
                    json!({
                        "surface": surface,
                        "agent": "claude",
                        "id": id,
                        "cwd": payload["cwd"],
                        "source": payload["source"],
                    }),
                    std::time::Duration::from_millis(500),
                )
                .await;
            }
            if ws.is_some() {
                let mut p = json!({"state": "idle"});
                if let Some(cwd) = payload["cwd"].as_str() {
                    p["summary"] = json!(format!("session started in {cwd}"));
                }
                let _ = call(socket, methods::AGENT_STATE, p).await;
            }
        }
        ClaudeHook::PromptSubmit => {
            if ws.is_some() {
                let mut p = json!({"state": "running"});
                if let Some(prompt) = payload["prompt"].as_str() {
                    p["summary"] = json!(truncate(prompt, 120));
                }
                let _ = call(socket, methods::AGENT_STATE, p).await;
            }
        }
        ClaudeHook::Stop => {
            if ws.is_some() {
                let mut p = json!({"state": "idle"});
                if let Some(s) = summary(&payload) {
                    p["summary"] = json!(s);
                }
                let _ = call(socket, methods::AGENT_STATE, p).await;
            }
        }
        ClaudeHook::Notification => {
            let message = payload["message"]
                .as_str()
                .unwrap_or("Claude needs input")
                .to_string();
            // The daemon finds the workspace from the caller's surface, which
            // stays right when the tab has moved to another workspace.
            let p = json!({
                "title": "Claude Code",
                "body": message,
                "surface": std::env::var("LMUX_SURFACE_ID").ok(),
            });
            let _ = call(socket, methods::NOTIFY, p).await;
            if ws.is_some() {
                let _ = call(
                    socket,
                    methods::AGENT_STATE,
                    json!({"state": "needs_input"}),
                )
                .await;
            }
        }
        ClaudeHook::PrintConfig => {}
    }
    // Hooks print nothing so Claude Code proceeds normally.
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        return s;
    }
    let t: String = s.chars().take(n).collect();
    format!("{t}…")
}

fn summarize_transcript(payload: &Value) -> Option<String> {
    let path = payload["transcript_path"].as_str()?;
    let data = std::fs::read_to_string(path).ok()?;
    let mut last: Option<String> = None;
    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["type"].as_str() != Some("assistant") {
            continue;
        }
        let content = &v["message"]["content"];
        if let Some(arr) = content.as_array() {
            for item in arr {
                if item["type"].as_str() == Some("text") {
                    if let Some(t) = item["text"].as_str() {
                        if !t.trim().is_empty() {
                            last = Some(truncate(t, 160));
                        }
                    }
                }
            }
        }
    }
    last
}

fn claude_settings_snippet() -> String {
    serde_json::to_string_pretty(&json!({
        "hooks": {
            "SessionStart": [{"matcher": "startup|resume|clear|compact|fork", "hooks": [{"type": "command", "command": "lmux hooks claude session-start"}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "lmux hooks claude prompt-submit"}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "lmux hooks claude stop"}]}],
            "Notification": [{"hooks": [{"type": "command", "command": "lmux hooks claude notification"}]}]
        }
    }))
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// websocket diagnostic client
// ---------------------------------------------------------------------------

/// Find a terminal surface's current size in a tree (used by the probe to show
/// that a resize is reflected in the next tree broadcast).
fn surface_dims(tree: &lmux_proto::Tree, surface: &str) -> Option<(u16, u16)> {
    for w in &tree.workspaces {
        for p in &w.panes {
            for s in &p.surfaces {
                if s.id == surface {
                    if let lmux_proto::SurfaceKind::Terminal { cols, rows, .. } = &s.kind {
                        return Some((*cols, *rows));
                    }
                }
            }
        }
    }
    None
}

async fn ws_probe(addr: &str, surface: &str, timeout_secs: u64, resizes: u64) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use lmux_proto::frame::Frame;
    use tokio_tungstenite::tungstenite::Message;

    let url = format!("ws://{addr}");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    let (mut sink, mut source) = ws.split();

    let hello = ClientMsg::Hello {
        protocol_version: lmux_proto::PROTOCOL_VERSION,
        client_name: "lmux-ws-probe".into(),
    };
    sink.send(Message::Text(serde_json::to_string(&hello)?)).await?;
    let sub = ClientMsg::Subscribe {
        surface: surface.to_string(),
        history: None,
    };
    sink.send(Message::Text(serde_json::to_string(&sub)?)).await?;
    println!("subscribed to {surface}");

    let mut got_snapshot = false;
    let mut output_bytes = 0usize;
    let mut sent_input = false;
    let mut tree_msgs = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, source.next()).await {
            Err(_) => break,
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                println!("ws error: {e}");
                break;
            }
            Ok(Some(Ok(msg))) => match msg {
                Message::Text(t) => {
                    if let Ok(sm) = serde_json::from_str::<ServerMsg>(&t) {
                        match sm {
                            ServerMsg::Welcome { host, .. } => println!("welcome host={host}"),
                            ServerMsg::TermSnapshot { surface, cols, rows, data } => {
                                got_snapshot = true;
                                let preview: String = data.chars().take(200).collect();
                                println!(
                                    "term_snapshot {surface} {cols}x{rows} ({} bytes) preview={:?}",
                                    data.len(),
                                    preview
                                );
                                if !sent_input {
                                    sent_input = true;
                                    let input = ClientMsg::TermInput {
                                        surface: surface.clone(),
                                        data: "echo WS_PROBE_MARKER\r".into(),
                                    };
                                    sink.send(Message::Text(serde_json::to_string(&input)?)).await?;
                                    let resize = ClientMsg::TermResize {
                                        surface: surface.clone(),
                                        cols: 100,
                                        rows: 30,
                                    };
                                    sink.send(Message::Text(serde_json::to_string(&resize)?))
                                        .await?;
                                    // Extra rapid resizes: with coalescing these
                                    // must not each produce a Tree.
                                    for i in 1..resizes {
                                        let resize = ClientMsg::TermResize {
                                            surface: surface.clone(),
                                            cols: 100 + i as u16,
                                            rows: 30,
                                        };
                                        sink.send(Message::Text(serde_json::to_string(&resize)?))
                                            .await?;
                                    }
                                    println!("sent term_input + {resizes} term_resize");
                                }
                            }
                            ServerMsg::TermExited { surface } => {
                                println!("term_exited {surface}");
                                break;
                            }
                            ServerMsg::Tree { tree } => {
                                tree_msgs += 1;
                                if let Some((cols, rows)) = surface_dims(&tree, surface) {
                                    println!("tree: {surface} is {cols}x{rows}");
                                }
                            }
                            other => println!("server: {other:?}"),
                        }
                    }
                }
                Message::Binary(b) => {
                    if let Ok(frame) = Frame::decode(&b) {
                        match frame {
                            Frame::TermOutput { surface, data } => {
                                output_bytes += data.len();
                                let text = String::from_utf8_lossy(data);
                                if text.contains("WS_PROBE_MARKER") {
                                    print!("term_output[{surface}] contains marker: ");
                                    println!("{:?}", text.trim_end());
                                }
                            }
                            Frame::BrowserFrame { .. } => {}
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            },
        }
    }

    println!(
        "probe summary: snapshot={got_snapshot} input_sent={sent_input} output_bytes={output_bytes} tree_msgs={tree_msgs}"
    );
    Ok(())
}

/// `$HOME`, where the installer puts the WebKit helper.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
