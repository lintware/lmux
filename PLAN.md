# lmux — plan

lmux is a leaner cmux. It keeps the parts of cmux that matter day to day (workspaces in folders, split terminals and browsers, notifications, and agent control from a CLI) and adds what cmux lacks. Everything runs on a server and keeps running when the laptop disconnects, browsers included.

## The user flow this must support

1. Open lmux on the laptop. It connects to `my-server` over Tailscale.
2. In the sidebar, open a folder (e.g. `webapp`) and a workspace in it. Start Claude Code in a terminal pane. The project files and Claude both live on the server.
3. Claude opens a browser pane, navigates, clicks and types. You watch it live in the app.
4. Close the laptop lid. Claude keeps working, and so does its browser.
5. Reopen the laptop. Every workspace, terminal screen and browser is exactly where the agent left it. Unread notifications say what happened.
6. A remote terminal or browser costs the laptop almost nothing: hidden panes stream nothing, and visible panes only receive screen updates.

## Target machine

`my-server` (hostname `my-server`, Tailscale `100.x.y.z`, SSH user `me`). Checked on 2026-10-09:

| | |
|---|---|
| OS | Debian 12 (bookworm), x86_64, kernel 6.1 |
| CPU / RAM | 8 cores, 31 GB (about 20 GB available) |
| Disk | 565 GB free |
| Installed | cargo/rustc 1.88, node 22, bun, tmux 3.3a, docker, claude, Chromium 154 (`apt install chromium`, installed 2026-10-09; headless verified) |
| systemd | user lingering is on, so `systemctl --user` services survive logout |

## Architecture

```
 Laptop (macOS)                                Server (my-server)
┌──────────────────────────┐   Tailscale    ┌─────────────────────────────────────────┐
│ lmux app (Tauri)         │   SSH tunnel   │ lmuxd  (Rust, systemd --user service)   │
│  sidebar: folders/ws     │◀══════════════▶│  session tree: folder▸workspace▸pane    │
│  xterm.js per visible    │  websocket:    │  terminals = tmux -L lmux (control mode) │
│    terminal pane         │  JSON control  │  Chromium (headless) via CDP             │
│  <canvas> per visible    │  + binary      │  notifications / status / log            │
│    browser pane          │  frames        │  state snapshot on disk                  │
└──────────────────────────┘                │         ▲ unix socket (JSON-RPC)         │
                                            │  `lmux` CLI ── used by you and agents   │
                                            │  Claude Code hooks ─▶ `lmux hooks`      │
                                            └─────────────────────────────────────────┘
```

### Why this shape

- **The daemon owns all state**, like a tmux server. The client is a viewer that can come and go. This is what makes "close the laptop, keep going" work.
- **tmux is the terminal backbone.** lmuxd runs a private tmux server (`tmux -L lmux`) and talks to it in control mode (`tmux -C`), which streams structured per-pane output and events. tmux owns the PTYs, scrollback and screen state, so terminals and Claude sessions survive even an lmuxd crash or upgrade. Every terminal surface is its own tmux window with a manual size, because lmux's layout mixes terminals and browsers, which tmux can't. Snapshots on reattach and agents' `read-screen` come from `capture-pane`, and input goes through `send-keys`.
- **lmuxd owns everything tmux can't:** the folder/workspace/pane layout, browsers, notifications, status and agent state. It stores its own metadata in tmux user options (`@lmux_*`) plus a state file, so after a restart it can rebuild the tree from the tmux sessions that are still running.
- **Browsers are headless Chromium on the server, driven over CDP** (the `chromiumoxide` crate). The agent's commands and your view use the same page, so what you see is what the agent is doing.
- **The laptop only pays for what's on screen.** The client subscribes to the panes it's showing. Hidden terminals send nothing. Hidden browsers stop their screencast, which also saves server CPU. Browser frames are paced by CDP's frame acknowledgements, so a slow link drops frames instead of building a backlog.
- **One Rust binary on the server:** `lmux server` runs the daemon, and every other `lmux …` command is the CLI. Copy one file and install one systemd unit.

### Transport and security

- v1: the app opens an SSH connection to the server (reusing your existing keys and Tailscale) and forwards the daemon's unix socket. lmuxd never listens on a network port.
- The socket is mode `0600` and owned by your user. Anyone who can reach it can run shell commands as you, so it is never exposed beyond SSH.
- Later: a direct Tailscale connection that authenticates with the Tailscale peer identity, to drop the SSH hop.

### Local workspaces

The laptop can run its own `lmuxd` (the same binary builds for macOS). The app then shows both machines in one sidebar, with folders grouped by host. Local work costs local resources, and remote work doesn't.

## Feature scope

### Kept from cmux (v1)

Command names match cmux wherever possible, so Claude's existing cmux habits and skills carry over with `cmux` replaced by `lmux`.

**Organize**
- Folders containing workspaces, as in the cmux sidebar. Each workspace card shows title, a summary of the last agent message, status (Running, Idle, Needs input), git branch and cwd.
- `new-workspace`, `rename-workspace`, `close-workspace`, `select-workspace`, `reorder-workspace`, `list-workspaces`, `tree`, `identify`.

**Panes and surfaces**
- Splits with terminal and browser panes, plus tabs (surfaces) inside a pane.
- `new-split`, `new-pane --type terminal|browser`, `new-surface`, `close-surface`, `focus-pane`, `move-surface`, `resize-pane`, `swap-pane`, `list-panes`, `list-pane-surfaces`.

**Terminal control for agents**
- `send`, `send-key`, `paste`, `read-screen [--scrollback] [--lines n]`, `capture-pane`, `wait-for`, `clear-history`.

**Browser control for agents**
- `browser open|open-split|goto|back|forward|reload|url`
- `browser snapshot [-i]` (accessibility tree with element refs), `click`, `dblclick`, `hover`, `type`, `fill`, `press`, `select`, `scroll`, `wait`, `eval`
- `browser screenshot`, `get`, `is`, `find`, `console list`, `errors list`, `tab new|list|switch|close`, `cookies`, `storage`

**Agent awareness**
- `notify`, `list-notifications`, `clear-notifications`, `jump-to-unread`
- `set-status`, `clear-status`, `set-progress`, `clear-progress`, `log`, `list-log`
- `lmux hooks setup` installs Claude Code hooks (session start, prompt submit, stop, notification) so workspace status and summaries update without the agent doing anything.
- Each pane's environment includes `LMUX_SOCKET`, `LMUX_WORKSPACE_ID` and `LMUX_SURFACE_ID`, so an agent can address "my own workspace" or "the pane next to me."

### Added beyond cmux

- Server-side persistence for terminals and browsers, with reattach.
- Browsers that keep running and stay watchable while the laptop is away.
- A multi-host sidebar.

### Left out to stay lean

cmux has a lot that lmux deliberately skips in v1: cloud VMs, the iOS simulator, vault and checkpoints, the diff viewer, recordings, the markdown viewer, automations, sidebar extensions, team features, and browser import and profiles beyond a default profile. Any of these can be added later if you miss them.

## Repo layout

```
lmux/
  Cargo.toml                 workspace
  crates/
    lmux-proto/              shared types: session tree, RPC requests/events, frame headers
    lmux-core/               session tree, tmux control-mode backend, persistence
    lmux-browser/            Chromium lifecycle, CDP sessions, screencast, agent browser ops
    lmux/                    the binary: `lmux server` + all CLI commands
  app/                       Tauri client
    src-tauri/               Rust side: SSH tunnel, websocket client, OS notifications
    src/                     web UI: sidebar, split layout, xterm.js panes, canvas browser panes
  deploy/
    lmuxd.service            systemd --user unit
  PLAN.md
```

## Protocol

The source of truth is `crates/lmux-proto` (`lib.rs` for messages, `methods.rs` for RPC names, `frame.rs` for binary frames). The summary below is a guide. Two protocols carry these types.

**CLI ⇄ daemon:** JSON-RPC over the unix socket, one request per connection. For example, `{"method":"surface.send","params":{"surface":"surface:3","text":"npm test\n"}}`.

**App ⇄ daemon:** one websocket carrying JSON control messages and binary frames.

| Direction | Message |
|---|---|
| app → daemon | `hello{client_version}`, `subscribe{surface}`, `unsubscribe{surface}`, `input{surface, bytes}`, `resize{surface, cols, rows}` or `{w, h}`, `browser_input{surface, mouse/key/wheel event}`, any RPC method |
| daemon → app | `tree{…}` (full tree on connect, then diffs), `term_snapshot{surface, screen}`, `term_output{surface, bytes}`, `browser_frame{surface, jpeg, metadata}` (binary), `browser_meta{url, title, loading}`, `notification{…}`, `status{…}` |

The browser canvas in the app is sized to its pane. The daemon starts the screencast at that size, so frames never carry more pixels than are displayed.

## Milestones

Each milestone ends with something you can run.

**M1 — Daemon, terminals, CLI (no GUI).**
lmuxd with the session tree, PTYs, server-side screen state, the unix socket and the core CLI (`new-workspace`, `new-split`, `send`, `send-key`, `read-screen`, `close-surface`, `tree`, `list-*`), installed as a systemd user service.
*Done when:* over plain SSH, `lmux new-workspace --command claude` starts Claude, you disconnect, reconnect, and `lmux read-screen` shows Claude's current screen.

**M2 — App attach.**
The Tauri app with the SSH tunnel, the sidebar with folders and workspaces, split layout, and xterm.js panes, including subscribe and unsubscribe and snapshot on attach.
*Done when:* you close the laptop with Claude mid-task, reopen it, and the app reconnects to the same screen in under 2 seconds. A hidden pane generates no network traffic.

**M3 — Browser.**
Chromium on the server, browser panes, screencast to the canvas, your mouse and keyboard input forwarded back, and the `lmux browser …` commands for agents on the same page.
*Done when:* Claude runs `lmux browser open http://localhost:3000`, then `snapshot -i` and `click`, and you watch it happen live. Closing and reopening the laptop shows the same page and state.

**M4 — Agent awareness.**
Notifications (in the app and as macOS notifications), Claude Code hooks, workspace status and summaries in the sidebar, and `set-status`, `set-progress` and `log`.
*Done when:* Claude finishing a task while the laptop is closed shows up as an unread notification and an "Idle" workspace on reopen.

**M5 — Durability and polish.**
State snapshot to disk so a daemon restart restores the layout (and offers `claude --resume` in agent panes), memory limits for browsers (suspend tabs hidden for long periods), keybindings, a local lmuxd for laptop workspaces, and the multi-host sidebar.

## Risks to test early

- **Screencast latency and bandwidth over Tailscale.** Prototype this in the first week of M3, before building browser chrome around it. If JPEG frames are too heavy, the fallbacks are a lower frame rate when idle, a lower quality while the page is scrolling, or later a WebRTC video track.
- **Chromium memory on a 31 GB box that already uses 11 GB.** Cap live browser tabs per workspace and suspend hidden ones, as cmux's memory saver does.
- **Headless detection.** Some sites behave differently in headless Chrome. If that bites, run headful Chromium against a virtual display (Xvfb) on the server instead. Nothing else in the design changes.
- **xterm.js speed versus Ghostty.** Fine for typical agent output; enable the WebGL renderer, and revisit if huge outputs lag.
- **Clipboard and file drops across machines.** The app needs to copy across explicitly, since the clipboard lives on the laptop and the files on the server.

## Decisions taken in this plan (open to change)

- **tmux as the terminal backbone**, with lmuxd layered on top for layout, browsers and agent features.
- Rust for the daemon and CLI, and **Tauri** for the client (Rust plus a web UI with xterm.js). The alternatives were a native Rust UI (more work) or a Swift client like cmux (Mac only, two languages).
- CLI command names that **match cmux**.
- **SSH-tunnelled** transport for v1, not a listening port.
- **Headless Chromium** first, with headful Chromium under Xvfb as the fallback.
