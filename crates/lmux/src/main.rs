//! The `lmux` binary: `lmux server` runs the daemon; every other command is a
//! thin JSON-RPC client over the daemon's unix socket.

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use lmux_proto::{methods, ClientMsg, RpcRequest, RpcResponse, ServerMsg};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

mod browser_adapter;

#[derive(Parser, Debug)]
#[command(name = "lmux", version, about = "lmux client and daemon")]
struct Cli {
    /// Path to the daemon unix socket (default $LMUX_SOCKET or ~/.local/state/lmux/lmux.sock).
    #[arg(long, global = true)]
    socket: Option<String>,
    /// Print raw JSON results.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Debug, Clone)]
struct ServerArgs {
    #[arg(long, default_value = "lmux")]
    tmux_socket: String,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:7878")]
    ws_addr: String,
    /// Run terminals only; every `browser.*` answers Unsupported.
    #[arg(long)]
    no_browser: bool,
    /// Chromium binary (default /usr/bin/chromium).
    #[arg(long)]
    chromium_path: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the daemon in the foreground.
    Server(ServerArgs),

    // organize
    NewFolder {
        name: String,
        /// Nest the folder inside this one, instead of the top level.
        #[arg(long)]
        parent: Option<String>,
    },
    MoveFolder {
        folder: Option<String>,
        /// Put the folder inside this one; omit to move it to the top level.
        #[arg(long)]
        parent: Option<String>,
        /// Slot among that parent's folders (default: last).
        #[arg(long)]
        index: Option<u64>,
    },
    NewWorkspace {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        folder: Option<String>,
    },
    RenameWorkspace {
        title: String,
        #[arg(long)]
        workspace: Option<String>,
    },
    CloseWorkspace {
        #[arg(long)]
        workspace: Option<String>,
    },
    SelectWorkspace {
        workspace: Option<String>,
    },
    ReorderWorkspace {
        workspace: Option<String>,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        index: Option<u64>,
    },
    ListWorkspaces,
    Tree,
    Identify,

    // panes & surfaces
    NewSplit {
        #[arg(value_parser = ["left", "right", "up", "down"])]
        side: String,
        #[arg(long)]
        surface: Option<String>,
        #[arg(long, name = "type", default_value = "terminal")]
        kind: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
    },
    NewPane {
        #[arg(long, name = "type", default_value = "terminal")]
        kind: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        side: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    NewSurface {
        #[arg(long)]
        pane: Option<String>,
        #[arg(long, name = "type", default_value = "terminal")]
        kind: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        command: Option<String>,
    },
    CloseSurface {
        #[arg(long)]
        surface: Option<String>,
    },
    /// Move a tab to another pane, or reorder it within its own.
    MoveSurface {
        surface: String,
        #[arg(long)]
        pane: String,
        #[arg(long)]
        index: Option<u64>,
    },
    /// Move a tab into a new pane split off `--pane`, or off its own pane.
    SplitSurface {
        surface: String,
        #[arg(long, value_parser = ["left", "right", "up", "down"])]
        side: String,
        /// The pane the split comes off; defaults to the tab's own pane.
        #[arg(long)]
        pane: Option<String>,
    },
    /// Exchange two panes' positions in the layout.
    SwapPane {
        pane: String,
        other: String,
    },
    FocusPane {
        pane: String,
    },
    ResizePane {
        pane: String,
        #[arg(long)]
        ratio: f32,
    },
    ListPanes {
        #[arg(long)]
        workspace: Option<String>,
    },

    // terminal control
    Send {
        #[arg(trailing_var_arg = true, num_args = 0..)]
        text: Vec<String>,
        #[arg(long)]
        surface: Option<String>,
        /// Append a newline (Enter).
        #[arg(long)]
        enter: bool,
    },
    SendKey {
        key: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Paste {
        #[arg(trailing_var_arg = true, num_args = 0..)]
        text: Vec<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    ReadScreen {
        #[arg(long)]
        scrollback: bool,
        #[arg(long)]
        lines: Option<u32>,
        #[arg(long)]
        surface: Option<String>,
    },

    // agent awareness
    Notify {
        title: String,
        #[arg(long)]
        subtitle: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    ListNotifications,
    ClearNotifications {
        #[arg(long)]
        workspace: Option<String>,
    },
    SetStatus {
        key: String,
        value: String,
        #[arg(long)]
        icon: Option<String>,
        #[arg(long)]
        color: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    ClearStatus {
        key: String,
        #[arg(long)]
        workspace: Option<String>,
    },
    SetProgress {
        value: f32,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    ClearProgress {
        #[arg(long)]
        workspace: Option<String>,
    },
    Log {
        message: String,
        #[arg(long, default_value = "info")]
        level: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    ListLog {
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        limit: Option<u64>,
    },

    // browser (routed to the browser backend)
    Browser {
        #[command(subcommand)]
        cmd: BrowserCmd,
    },

    // Claude Code hooks
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },

    /// Diagnostic: connect to the websocket, subscribe, print snapshot/output.
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

#[derive(Subcommand, Debug)]
enum BrowserCmd {
    Open {
        url: Option<String>,
        #[arg(long)]
        side: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    OpenSplit {
        url: Option<String>,
        #[arg(long)]
        side: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    Goto {
        url: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Back {
        #[arg(long)]
        surface: Option<String>,
    },
    Forward {
        #[arg(long)]
        surface: Option<String>,
    },
    Reload {
        #[arg(long)]
        surface: Option<String>,
    },
    Url {
        #[arg(long)]
        surface: Option<String>,
    },
    Snapshot {
        #[arg(long)]
        surface: Option<String>,
        #[arg(short, long)]
        interactive: bool,
    },
    Click {
        target: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Dblclick {
        target: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Hover {
        target: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Type {
        target: String,
        text: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Fill {
        target: String,
        text: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Press {
        key: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Select {
        target: String,
        value: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Scroll {
        #[arg(long)]
        dx: Option<f64>,
        #[arg(long)]
        dy: Option<f64>,
        #[arg(long)]
        surface: Option<String>,
    },
    Wait {
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        url_contains: Option<String>,
        #[arg(long)]
        load_state: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
        #[arg(long)]
        surface: Option<String>,
    },
    Eval {
        script: String,
        #[arg(long)]
        surface: Option<String>,
    },
    Screenshot {
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    Get {
        what: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    Console {
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        surface: Option<String>,
    },
    Errors {
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        surface: Option<String>,
    },
    Tab {
        action: String,
        #[arg(long)]
        index: Option<u64>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum HooksCmd {
    /// Install/print Claude Code hooks.
    Claude {
        #[command(subcommand)]
        cmd: ClaudeHook,
    },
}

#[derive(Subcommand, Debug)]
enum ClaudeHook {
    SessionStart,
    PromptSubmit,
    Stop,
    Notification,
    /// Print the settings.json snippet (does not edit anything).
    PrintConfig,
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
    let cli = Cli::parse();
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
                tmux_socket: args.tmux_socket,
                ws_addr: args.ws_addr,
                host: lmux_core::daemon::hostname(),
            };
            // Browser backend is wired in here: this is the only crate that
            // sees both the daemon's trait and the engine.
            let browser: Option<std::sync::Arc<dyn lmux_core::BrowserBackend>> = if args.no_browser {
                tracing::info!("browser backend disabled (--no-browser)");
                None
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
        Cmd::Hooks { cmd } => {
            hooks(cmd, &socket, json).await?;
            return Ok(());
        }
        other => {
            let (method, params, printer) = build_request(other);
            let result = call(&socket, method, params).await?;
            printer(&result, json);
            Ok(())
        }
    }
}

type Printer = Box<dyn Fn(&Value, bool)>;

fn print_json_or(v: &Value, json: bool, human: impl FnOnce(&Value)) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()));
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

fn build_request(cmd: Cmd) -> (&'static str, Value, Printer) {
    match cmd {
        Cmd::NewFolder { name, parent } => (
            methods::FOLDER_NEW,
            json!({"name": name, "parent": parent}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["folder"])))),
        ),
        Cmd::MoveFolder { folder, parent, index } => (
            methods::FOLDER_MOVE,
            json!({"folder": folder, "parent": parent, "index": index}),
            ok_box(),
        ),
        Cmd::NewWorkspace { name, cwd, command, folder } => (
            methods::WORKSPACE_NEW,
            json!({"name": name, "cwd": cwd, "command": command, "folder": folder}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    println!("{}", refs_line(v, &["workspace", "pane", "surface"]))
                })
            }),
        ),
        Cmd::RenameWorkspace { title, workspace } => (
            methods::WORKSPACE_RENAME,
            json!({"title": title, "workspace": workspace}),
            ok_box(),
            ),
        Cmd::CloseWorkspace { workspace } => (
            methods::WORKSPACE_CLOSE,
            json!({"workspace": workspace}),
            ok_box(),
            ),
        Cmd::SelectWorkspace { workspace } => (
            methods::WORKSPACE_SELECT,
            json!({"workspace": workspace}),
            ok_box(),
            ),
        Cmd::ReorderWorkspace { workspace, folder, index } => (
            methods::WORKSPACE_MOVE,
            json!({"workspace": workspace, "folder": folder, "index": index}),
            ok_box(),
            ),
        Cmd::ListWorkspaces => (
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
        Cmd::Tree => (
            methods::TREE,
            json!({}),
            Box::new(|v, j| {
                if j {
                    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
                } else {
                    print_tree(v);
                }
            }),
        ),
        Cmd::Identify => (
            methods::IDENTIFY,
            json!({}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["workspace", "pane", "surface"])))),
        ),
        Cmd::NewSplit { side, surface, kind, url, command, cwd } => (
            methods::SPLIT_NEW,
            json!({"side": side, "surface": surface, "type": kind, "url": url, "command": command, "cwd": cwd}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["pane", "surface"])))),
        ),
        Cmd::NewPane { kind, url, command, side, surface } => (
            methods::SPLIT_NEW,
            json!({"side": side.unwrap_or_else(|| "right".into()), "surface": surface, "type": kind, "url": url, "command": command}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["pane", "surface"])))),
        ),
        Cmd::NewSurface { pane, kind, url, command } => (
            methods::SURFACE_NEW,
            json!({"pane": pane, "type": kind, "url": url, "command": command}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["surface"])))),
        ),
        Cmd::CloseSurface { surface } => (
            methods::SURFACE_CLOSE,
            json!({"surface": surface}),
            ok_box(),
        ),
        Cmd::MoveSurface { surface, pane, index } => (
            methods::SURFACE_MOVE,
            json!({"surface": surface, "pane": pane, "index": index}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["pane"])))),
        ),
        Cmd::SplitSurface { surface, side, pane } => (
            methods::SURFACE_SPLIT,
            json!({"surface": surface, "side": side, "pane": pane}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["pane"])))),
        ),
        Cmd::SwapPane { pane, other } => (
            methods::PANE_SWAP,
            json!({"pane": pane, "other": other}),
            ok_box(),
        ),
        Cmd::FocusPane { pane } => (methods::PANE_FOCUS, json!({"pane": pane}), ok_box()),
        Cmd::ResizePane { pane, ratio } => (
            methods::PANE_RESIZE,
            json!({"pane": pane, "ratio": ratio}),
            ok_box(),
        ),
        Cmd::ListPanes { workspace } => (
            methods::PANE_LIST,
            json!({"workspace": workspace}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    for p in v["panes"].as_array().cloned().unwrap_or_default() {
                        let surfaces: Vec<String> = p["surfaces"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .iter()
                            .map(|s| s["id"].as_str().unwrap_or("?").to_string())
                            .collect();
                        println!(
                            "{}\tactive={}\t[{}]",
                            p["id"].as_str().unwrap_or("?"),
                            p["active_surface"].as_str().unwrap_or("-"),
                            surfaces.join(", ")
                        );
                    }
                })
            }),
        ),
        Cmd::Send { text, surface, enter } => {
            let mut t = text.join(" ");
            if enter {
                t.push('\n');
            }
            (
                methods::TERM_SEND,
                json!({"surface": surface, "text": t}),
                ok_box(),
            )
        }
        Cmd::Paste { text, surface } => (
            methods::TERM_SEND,
            json!({"surface": surface, "text": text.join(" ")}),
            ok_box(),
            ),
        Cmd::SendKey { key, surface } => (
            methods::TERM_SEND_KEY,
            json!({"surface": surface, "key": key}),
            ok_box(),
            ),
        Cmd::ReadScreen { scrollback, lines, surface } => (
            methods::TERM_READ_SCREEN,
            json!({"surface": surface, "scrollback": scrollback, "lines": lines}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    if let Some(t) = v["text"].as_str() {
                        println!("{t}");
                    }
                })
            }),
        ),
        Cmd::Notify { title, subtitle, body, workspace } => (
            methods::NOTIFY,
            json!({"title": title, "subtitle": subtitle, "body": body, "workspace": workspace}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["id"])))),
        ),
        Cmd::ListNotifications => (
            methods::NOTIFY_LIST,
            json!({}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    for n in v["notifications"].as_array().cloned().unwrap_or_default() {
                        println!(
                            "{}\t{}\tread={}",
                            n["id"].as_str().unwrap_or("?"),
                            n["title"].as_str().unwrap_or("?"),
                            n["read"].as_bool().unwrap_or(false)
                        );
                    }
                })
            }),
        ),
        Cmd::ClearNotifications { workspace } => (
            methods::NOTIFY_CLEAR,
            json!({"workspace": workspace}),
            ok_box(),
            ),
        Cmd::SetStatus { key, value, icon, color, workspace } => (
            methods::STATUS_SET,
            json!({"key": key, "value": value, "icon": icon, "color": color, "workspace": workspace}),
            ok_box(),
            ),
        Cmd::ClearStatus { key, workspace } => (
            methods::STATUS_CLEAR,
            json!({"key": key, "workspace": workspace}),
            ok_box(),
            ),
        Cmd::SetProgress { value, label, workspace } => (
            methods::PROGRESS_SET,
            json!({"value": value, "label": label, "workspace": workspace}),
            ok_box(),
            ),
        Cmd::ClearProgress { workspace } => (
            methods::PROGRESS_CLEAR,
            json!({"workspace": workspace}),
            ok_box(),
            ),
        Cmd::Log { message, level, source, workspace } => (
            methods::LOG,
            json!({"message": message, "level": level, "source": source, "workspace": workspace}),
            ok_box(),
            ),
        Cmd::ListLog { workspace, limit } => (
            methods::LOG_LIST,
            json!({"workspace": workspace, "limit": limit}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    for e in v["entries"].as_array().cloned().unwrap_or_default() {
                        println!(
                            "[{}] {}",
                            e["level"].as_str().unwrap_or("info"),
                            e["message"].as_str().unwrap_or("")
                        );
                    }
                })
            }),
        ),
        Cmd::Browser { cmd } => return build_browser_request(cmd),
        _ => unreachable!("handled elsewhere"),
    }
}

fn build_browser_request(cmd: BrowserCmd) -> (&'static str, Value, Printer) {
    match cmd {
        BrowserCmd::Open { url, side, surface } => (
            methods::BROWSER_OPEN,
            json!({"url": url, "side": side, "surface": surface}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["surface"])))),
        ),
        BrowserCmd::OpenSplit { url, side, surface } => (
            methods::BROWSER_OPEN,
            json!({"url": url, "side": side, "surface": surface}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["surface"])))),
        ),
        BrowserCmd::Goto { url, surface } => (methods::BROWSER_GOTO, json!({"url": url, "surface": surface}), ok_box()),
        BrowserCmd::Back { surface } => (methods::BROWSER_BACK, json!({"surface": surface}), ok_box()),
        BrowserCmd::Forward { surface } => (methods::BROWSER_FORWARD, json!({"surface": surface}), ok_box()),
        BrowserCmd::Reload { surface } => (methods::BROWSER_RELOAD, json!({"surface": surface}), ok_box()),
        BrowserCmd::Url { surface } => (
            methods::BROWSER_URL,
            json!({"surface": surface}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    println!(
                        "{}\t{}",
                        v["url"].as_str().unwrap_or(""),
                        v["title"].as_str().unwrap_or("")
                    )
                })
            }),
        ),
        BrowserCmd::Snapshot { surface, interactive } => (
            methods::BROWSER_SNAPSHOT,
            json!({"surface": surface, "interactive": interactive}),
            Box::new(|v, j| {
                print_json_or(v, j, |v| {
                    if let Some(t) = v["text"].as_str() {
                        println!("{t}");
                    }
                })
            }),
        ),
        BrowserCmd::Click { target, surface } => (methods::BROWSER_CLICK, json!({"target": target, "surface": surface}), ok_box()),
        BrowserCmd::Dblclick { target, surface } => (methods::BROWSER_DBLCLICK, json!({"target": target, "surface": surface}), ok_box()),
        BrowserCmd::Hover { target, surface } => (methods::BROWSER_HOVER, json!({"target": target, "surface": surface}), ok_box()),
        BrowserCmd::Type { target, text, surface } => (methods::BROWSER_TYPE, json!({"target": target, "text": text, "surface": surface}), ok_box()),
        BrowserCmd::Fill { target, text, surface } => (methods::BROWSER_FILL, json!({"target": target, "text": text, "surface": surface}), ok_box()),
        BrowserCmd::Press { key, surface } => (methods::BROWSER_PRESS, json!({"key": key, "surface": surface}), ok_box()),
        BrowserCmd::Select { target, value, surface } => (methods::BROWSER_SELECT, json!({"target": target, "value": value, "surface": surface}), ok_box()),
        BrowserCmd::Scroll { dx, dy, surface } => (methods::BROWSER_SCROLL, json!({"dx": dx, "dy": dy, "surface": surface}), ok_box()),
        BrowserCmd::Wait { selector, text, url_contains, load_state, timeout_ms, surface } => (
            methods::BROWSER_WAIT,
            json!({"selector": selector, "text": text, "url_contains": url_contains, "load_state": load_state, "timeout_ms": timeout_ms, "surface": surface}),
            Box::new(|v, j| print_json_or(v, j, |_| println!("OK"))),
        ),
        BrowserCmd::Eval { script, surface } => (
            methods::BROWSER_EVAL,
            json!({"script": script, "surface": surface}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", v["value"]))),
        ),
        BrowserCmd::Screenshot { path, surface } => (
            methods::BROWSER_SCREENSHOT,
            json!({"path": path, "surface": surface}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", refs_line(v, &["path"])))),
        ),
        BrowserCmd::Get { what, target, name, surface } => (
            methods::BROWSER_GET,
            json!({"what": what, "target": target, "name": name, "surface": surface}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", v["value"]))),
        ),
        BrowserCmd::Console { clear, surface } => (
            methods::BROWSER_CONSOLE,
            json!({"clear": clear, "surface": surface}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", v["entries"]))),
        ),
        BrowserCmd::Errors { clear, surface } => (
            methods::BROWSER_ERRORS,
            json!({"clear": clear, "surface": surface}),
            Box::new(|v, j| print_json_or(v, j, |v| println!("{}", v["entries"]))),
        ),
        BrowserCmd::Tab { action, index, url, surface } => (
            methods::BROWSER_TABS,
            json!({"action": action, "index": index, "url": url, "surface": surface}),
            ok_box(),
            ),
    }
}

fn ok_box() -> Printer {
    Box::new(ok_printer)
}

fn ok_printer(v: &Value, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()));
    } else {
        println!("OK");
    }
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
        print_layout(&w["layout"], 2);
    }
}

fn print_layout(layout: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match layout["type"].as_str() {
        Some("pane") => println!("{pad}pane {}", layout["pane"].as_str().unwrap_or("?")),
        Some("split") => {
            println!(
                "{pad}split {} ratio={}",
                layout["direction"].as_str().unwrap_or("?"),
                layout["ratio"].as_f64().unwrap_or(0.5)
            );
            print_layout(&layout["first"], indent + 2);
            print_layout(&layout["second"], indent + 2);
        }
        _ => {}
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
    let ws = std::env::var("LMUX_WORKSPACE_ID").ok();
    let summary = |payload: &Value| -> Option<String> {
        summarize_transcript(payload)
    };

    match cmd {
        ClaudeHook::SessionStart => {
            if let Some(ws) = ws {
                let mut p = json!({"state": "idle", "workspace": ws});
                if let Some(cwd) = payload["cwd"].as_str() {
                    p["summary"] = json!(format!("session started in {cwd}"));
                }
                let _ = call(socket, methods::AGENT_STATE, p).await;
            }
        }
        ClaudeHook::PromptSubmit => {
            if let Some(ws) = ws {
                let mut p = json!({"state": "running", "workspace": ws});
                if let Some(prompt) = payload["prompt"].as_str() {
                    p["summary"] = json!(truncate(prompt, 120));
                }
                let _ = call(socket, methods::AGENT_STATE, p).await;
            }
        }
        ClaudeHook::Stop => {
            if let Some(ws) = ws {
                let mut p = json!({"state": "idle", "workspace": ws});
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
            let mut p = json!({"title": "Claude Code", "body": message, "workspace": ws.clone()});
            if p["workspace"].is_null() {
                p.as_object_mut().unwrap().remove("workspace");
            }
            let _ = call(socket, methods::NOTIFY, p).await;
            if let Some(ws) = ws {
                let _ = call(
                    socket,
                    methods::AGENT_STATE,
                    json!({"state": "needs_input", "workspace": ws}),
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
            "SessionStart": [{"hooks": [{"type": "command", "command": "lmux hooks claude session-start"}]}],
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
